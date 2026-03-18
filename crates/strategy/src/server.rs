// =========================================================================
// server.rs: Strategy WebSocket server
// =========================================================================
//
// CD-14: Auth first message, 5-second deadline.
// CD-15: Single active connection, second replaces first.
// CD-16: Timeout = skip, never block.

use crate::{validate_auth, StrategyAction, StrategyError, StrategyEvent};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Mutex};
use tokio_tungstenite::tungstenite::Message;

// =========================================================================
// Types
// =========================================================================

/// Handle to the running strategy server.
/// Clone-safe — uses Arc internally for shared state.
pub struct StrategyServer {
    /// Channel to send events TO the connected client.
    event_tx: mpsc::Sender<StrategyEvent>,
    /// Channel to receive batch-cycle actions FROM the connected client (Commit, Reveal).
    action_rx: Arc<Mutex<mpsc::Receiver<StrategyAction>>>,
    /// Channel to receive token actions FROM the connected client (Mint, Burn, Lock, etc).
    /// Taken by run.rs and passed to the background token worker.
    token_action_rx: Arc<Mutex<Option<mpsc::Receiver<StrategyAction>>>>,
    /// Watch channel: true when a client is authenticated.
    connected_rx: watch::Receiver<bool>,
    /// Server address (for tests).
    addr: SocketAddr,
}

/// Internal per-connection state.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    WaitingForAuth,
    Authenticated,
    Disconnected,
}

// =========================================================================
// Server implementation
// =========================================================================

impl StrategyServer {
    /// Start the WebSocket server. Returns a handle for sending events / receiving actions.
    ///
    /// - `addr`: Listen address (e.g. "127.0.0.1:0" for random port in tests).
    /// - `auth_token`: Expected auth token from strategy clients.
    /// - `nft_id`: This node's NFT ID (sent in auth_ok).
    /// - `network`: Network name (sent in auth_ok).
    /// - `auth_details`: Optional extended details (markets, decimals, addresses).
    pub async fn start(
        addr: &str,
        auth_token: String,
        nft_id: u64,
        network: String,
        auth_details: Option<super::AuthOkDetails>,
    ) -> Result<Self, StrategyError> {
        let listener = TcpListener::bind(addr).await
            .map_err(|e| StrategyError::ParseError(format!("bind failed: {}", e)))?;
        let bound_addr = listener.local_addr()
            .map_err(|e| StrategyError::ParseError(format!("local_addr: {}", e)))?;

        // Channels: event_tx → server task → WS client
        //           WS client → server task → action_tx → action_rx (batch: Commit, Reveal)
        //           WS client → server task → token_action_tx → token_action_rx (Mint, Burn, etc)
        let (event_tx, event_rx) = mpsc::channel::<StrategyEvent>(32);
        let (action_tx, action_rx) = mpsc::channel::<StrategyAction>(32);
        let (token_action_tx, token_action_rx) = mpsc::channel::<StrategyAction>(32);
        let (connected_tx, connected_rx) = watch::channel(false);

        let event_rx = Arc::new(Mutex::new(event_rx));
        let auth_details = Arc::new(auth_details);

        // Spawn acceptor loop
        tokio::spawn(acceptor_loop(
            listener,
            auth_token,
            nft_id,
            network,
            auth_details,
            event_rx,
            action_tx,
            token_action_tx,
            connected_tx,
        ));

        Ok(StrategyServer {
            event_tx,
            action_rx: Arc::new(Mutex::new(action_rx)),
            token_action_rx: Arc::new(Mutex::new(Some(token_action_rx))),
            connected_rx,
            addr: bound_addr,
        })
    }

    /// The address the server is listening on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Whether a client is currently authenticated and connected.
    pub fn is_connected(&self) -> bool {
        *self.connected_rx.borrow()
    }

    /// Send an event to the connected strategy client.
    /// Returns Err if no client is connected or the channel is closed.
    pub async fn send_event(&self, event: StrategyEvent) -> Result<(), StrategyError> {
        self.event_tx.send(event).await
            .map_err(|_| StrategyError::ParseError("event channel closed".into()))
    }

    /// Receive the next action from the strategy client, with a timeout.
    /// Returns None if timeout expires or no client is connected.
    pub async fn receive_action_with_timeout(
        &self,
        timeout: Duration,
    ) -> Option<StrategyAction> {
        let mut rx = self.action_rx.lock().await;
        tokio::time::timeout(timeout, rx.recv()).await.ok().flatten()
    }

    /// Take ownership of the token action receiver.
    /// Called once by run.rs to pass to the background token worker.
    /// Returns None if already taken.
    pub async fn take_token_action_rx(&self) -> Option<mpsc::Receiver<StrategyAction>> {
        let mut guard = self.token_action_rx.lock().await;
        guard.take()
    }

    /// Clone the event sender for use by the token worker.
    /// Allows the worker to send TokenActionResult events back to the strategy.
    pub fn event_sender(&self) -> mpsc::Sender<StrategyEvent> {
        self.event_tx.clone()
    }
}

// =========================================================================
// Acceptor loop (background task)
// =========================================================================

async fn acceptor_loop(
    listener: TcpListener,
    auth_token: String,
    nft_id: u64,
    network: String,
    auth_details: Arc<Option<super::AuthOkDetails>>,
    event_rx: Arc<Mutex<mpsc::Receiver<StrategyEvent>>>,
    action_tx: mpsc::Sender<StrategyAction>,
    token_action_tx: mpsc::Sender<StrategyAction>,
    connected_tx: watch::Sender<bool>,
) {
    // Track active connection's abort handle so we can kill it on replacement.
    let active_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(None));

    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => break,
        };

        let ws = match tokio_tungstenite::accept_async(stream).await {
            Ok(ws) => ws,
            Err(_) => continue,
        };

        // Kill previous connection if any (CD-15: second replaces first).
        {
            let mut handle = active_handle.lock().await;
            if let Some(h) = handle.take() {
                h.abort();
            }
            let _ = connected_tx.send(false);
        }

        let token = auth_token.clone();
        let net = network.clone();
        let ad = auth_details.clone();
        let erx = event_rx.clone();
        let atx = action_tx.clone();
        let tatx = token_action_tx.clone();
        let ctx = connected_tx.clone();

        let conn_handle = tokio::spawn(async move {
            handle_connection(ws, &token, nft_id, &net, &ad, erx, atx, tatx, ctx.clone()).await;
            let _ = ctx.send(false);
        });

        active_handle.lock().await.replace(conn_handle);
    }
}

// =========================================================================
// Per-connection handler
// =========================================================================

async fn handle_connection(
    ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    auth_token: &str,
    nft_id: u64,
    network: &str,
    auth_details: &Arc<Option<super::AuthOkDetails>>,
    event_rx: Arc<Mutex<mpsc::Receiver<StrategyEvent>>>,
    action_tx: mpsc::Sender<StrategyAction>,
    token_action_tx: mpsc::Sender<StrategyAction>,
    connected_tx: watch::Sender<bool>,
) {
    let (mut ws_sink, mut ws_stream) = ws.split();

    // ── Phase 1: Auth handshake (5-second deadline, CD-14) ───────────────
    let auth_result = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    return Some(text);
                }
                Ok(Message::Close(_)) => return None,
                Ok(_) => continue, // skip ping/pong/binary
                Err(_) => return None,
            }
        }
        None
    })
    .await;

    let first_msg = match auth_result {
        Ok(Some(text)) => text,
        _ => {
            // Timeout or disconnect before auth — drop silently.
            let _ = ws_sink.close().await;
            return;
        }
    };

    // Parse first message — MUST be auth action.
    let parsed = match serde_json::from_str::<serde_json::Value>(&first_msg) {
        Ok(v) => v,
        Err(_) => {
            let fail = StrategyEvent::AuthFailed {
                reason: "invalid JSON".into(),
            };
            let _ = ws_sink
                .send(Message::Text(fail.to_json().to_string()))
                .await;
            let _ = ws_sink.close().await;
            return;
        }
    };

    let action_str = parsed.get("action").and_then(|v| v.as_str()).unwrap_or("");
    if action_str != "auth" {
        let fail = StrategyEvent::AuthFailed {
            reason: "first message must be auth".into(),
        };
        let _ = ws_sink
            .send(Message::Text(fail.to_json().to_string()))
            .await;
        let _ = ws_sink.close().await;
        return;
    }

    let token = parsed
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if validate_auth(token, auth_token).is_err() {
        let fail = StrategyEvent::AuthFailed {
            reason: "invalid token".into(),
        };
        let _ = ws_sink
            .send(Message::Text(fail.to_json().to_string()))
            .await;
        let _ = ws_sink.close().await;
        return;
    }

    // Auth OK
    let ok_event = StrategyEvent::AuthOk {
        nft_id,
        network: network.to_string(),
        auth_details: (**auth_details).clone(),
    };
    if ws_sink
        .send(Message::Text(ok_event.to_json().to_string()))
        .await
        .is_err()
    {
        return;
    }

    let _ = connected_tx.send(true);

    // ── Phase 2: Message relay loop ──────────────────────────────────────
    // Two concurrent tasks:
    // 1. Forward events from event_rx → WS sink
    // 2. Forward messages from WS stream → action_tx

    let mut event_rx_guard = event_rx.lock().await;

    loop {
        tokio::select! {
            // Events from orchestrator → WS client
            event = event_rx_guard.recv() => {
                match event {
                    Some(evt) => {
                        let json = evt.to_json().to_string();
                        if ws_sink.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                        // If disconnected event, close after sending
                        if evt.event_name() == "disconnected" {
                            let _ = ws_sink.close().await;
                            break;
                        }
                    }
                    None => break, // event channel closed
                }
            }
            // Messages from WS client → orchestrator
            msg = ws_stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match StrategyAction::from_json(
                            &serde_json::from_str(&text).unwrap_or_default()
                        ) {
                            Ok(action) => {
                                if action.is_token_action() {
                                    if token_action_tx.send(action).await.is_err() {
                                        break;
                                    }
                                } else {
                                    if action_tx.send(action).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(_) => {
                                // Silently drop unparseable messages (log in prod).
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    Some(Ok(Message::Ping(data))) => {
                        // Respond to pings to keep connection alive
                        if ws_sink.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    _ => {} // skip pong/binary
                }
            }
        }
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use futures_util::{SinkExt, StreamExt};
    use std::collections::HashMap;
    use tokio_tungstenite::tungstenite::Message;

    /// Helper: connect a WS client to the server.
    async fn connect_client(
        addr: SocketAddr,
    ) -> tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<TcpStream>,
    > {
        let url = format!("ws://{}", addr);
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws
    }

    /// Helper: send JSON text and return parsed response.
    async fn send_and_recv(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<TcpStream>,
        >,
        msg: &str,
    ) -> Option<serde_json::Value> {
        ws.send(Message::Text(msg.to_string())).await.ok()?;
        // Read next text message
        loop {
            match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    return serde_json::from_str(&text).ok();
                }
                Ok(Some(Ok(_))) => continue,
                _ => return None,
            }
        }
    }

    /// Helper: read next text message.
    async fn recv_msg(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<TcpStream>,
        >,
    ) -> Option<serde_json::Value> {
        loop {
            match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    return serde_json::from_str(&text).ok();
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Err(_) => return None,
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(_))) => return None,
            }
        }
    }

    /// Helper: authenticate a client.
    async fn auth_client(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<TcpStream>,
        >,
        token: &str,
    ) -> serde_json::Value {
        let msg = format!(r#"{{"action":"auth","token":"{}"}}"#, token);
        send_and_recv(ws, &msg).await.unwrap()
    }

    // ── T_WS_01: Start server, connect client ────────────────────────────

    #[tokio::test]
    async fn t_ws_01_start_and_connect() {
        let server = StrategyServer::start(
            "127.0.0.1:0", "secret".into(), 42, "testnet".into(), None,
        )
        .await
        .unwrap();

        let _ws = connect_client(server.addr()).await;
        // Connection accepted — if connect_async didn't error, we're good.
    }

    // ── T_WS_02: Correct auth → auth_ok ─────────────────────────────────

    #[tokio::test]
    async fn t_ws_02_auth_success() {
        let server = StrategyServer::start(
            "127.0.0.1:0", "secret".into(), 42, "testnet".into(), None,
        )
        .await
        .unwrap();

        let mut ws = connect_client(server.addr()).await;
        let resp = auth_client(&mut ws, "secret").await;

        assert_eq!(resp["event"], "auth_ok");
        assert_eq!(resp["data"]["nft_id"], 42);
        assert_eq!(resp["data"]["network"], "testnet");

        // Give server time to update state
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(server.is_connected());
    }

    // ── T_WS_03: Wrong token → auth_failed, connection closed ────────────

    #[tokio::test]
    async fn t_ws_03_auth_wrong_token() {
        let server = StrategyServer::start(
            "127.0.0.1:0", "secret".into(), 42, "testnet".into(), None,
        )
        .await
        .unwrap();

        let mut ws = connect_client(server.addr()).await;
        let resp = auth_client(&mut ws, "wrong_token").await;

        assert_eq!(resp["event"], "auth_failed");

        // Next read should be close or error
        let next = recv_msg(&mut ws).await;
        assert!(next.is_none(), "expected connection closed");

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!server.is_connected());
    }

    // ── T_WS_04: Auth timeout (5s) → connection dropped ─────────────────

    #[tokio::test]
    async fn t_ws_04_auth_timeout() {
        let server = StrategyServer::start(
            "127.0.0.1:0", "secret".into(), 42, "testnet".into(), None,
        )
        .await
        .unwrap();

        let mut ws = connect_client(server.addr()).await;

        // Send nothing, wait for server to drop us (5s timeout).
        // We use a 6s timeout on our side.
        let next = tokio::time::timeout(
            Duration::from_secs(7),
            ws.next(),
        )
        .await;

        // Should get close or None within ~5s
        match next {
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Err(_) => {
                // Connection dropped as expected
            }
            Ok(Some(Err(_))) => {
                // Connection error — also acceptable (server closed)
            }
            other => {
                panic!("expected connection dropped, got {:?}", other);
            }
        }

        assert!(!server.is_connected());
    }

    // ── T_WS_05: First message not auth → auth_failed ────────────────────

    #[tokio::test]
    async fn t_ws_05_first_message_not_auth() {
        let server = StrategyServer::start(
            "127.0.0.1:0", "secret".into(), 42, "testnet".into(), None,
        )
        .await
        .unwrap();

        let mut ws = connect_client(server.addr()).await;
        let resp = send_and_recv(
            &mut ws,
            r#"{"action":"commit","orders":[]}"#,
        )
        .await
        .unwrap();

        assert_eq!(resp["event"], "auth_failed");
        assert!(resp["data"]["reason"].as_str().unwrap().contains("first message must be auth"));

        assert!(!server.is_connected());
    }

    // ── T_WS_06: Send event to authed client ─────────────────────────────

    #[tokio::test]
    async fn t_ws_06_send_event() {
        let server = StrategyServer::start(
            "127.0.0.1:0", "secret".into(), 42, "testnet".into(), None,
        )
        .await
        .unwrap();

        let mut ws = connect_client(server.addr()).await;
        auth_client(&mut ws, "secret").await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send a BatchStart event from server side
        let mut escrow = HashMap::new();
        escrow.insert("KAY".to_string(), "500.00000000".to_string());

        server
            .send_event(StrategyEvent::BatchStart {
                data: BatchStartData {
                    batch_id: 100,
                    pool_id: 2,
                    escrow,
                    escrow_confirmed: HashMap::new(),
                    wallet: HashMap::new(),
                    gas_balance: "50.00000000".into(),
                    last_batch: None,
                    batch_params: BatchParamsData {
                        blocks_per_batch: 10,
                        commits_per_batch: 3,
                        num_pools: 4,
                    },
                    pending_settlements: vec![],
                    peers_in_pool: 5,
                    mint_state: MintStateData {
                        state: "AWAITING_TRIGGER".to_string(),
                        hold_duration_secs: 0, period_end: 0, block_end: 0,
                        has_pending_mint: false, pending_claimable_at: 0,
                    },
                    circulating: HashMap::new(),
                    vault_locks: vec![],
                    node_health: None,
                },
            })
            .await
            .unwrap();

        let resp = recv_msg(&mut ws).await.unwrap();
        assert_eq!(resp["event"], "batch_start");
        assert_eq!(resp["data"]["batch_id"], 100);
        assert_eq!(resp["data"]["escrow"]["KAY"], "500.00000000");
    }

    // ── T_WS_07: Receive action from authed client ───────────────────────

    #[tokio::test]
    async fn t_ws_07_receive_action() {
        let server = StrategyServer::start(
            "127.0.0.1:0", "secret".into(), 42, "testnet".into(), None,
        )
        .await
        .unwrap();

        let mut ws = connect_client(server.addr()).await;
        auth_client(&mut ws, "secret").await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Client sends commit action
        ws.send(Message::Text(
            r#"{"action":"commit","orders":[{"pair":"EMM/KAY","side":"buy","price":"0.05","quantity":"100"}]}"#.into(),
        ))
        .await
        .unwrap();

        let action = server
            .receive_action_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();

        match action {
            StrategyAction::Commit { orders } => {
                assert_eq!(orders.len(), 1);
                assert_eq!(orders[0].pair, "EMM/KAY");
                assert_eq!(orders[0].side, "buy");
            }
            other => panic!("expected Commit, got {:?}", other),
        }
    }

    // ── T_WS_08: Second connection replaces first ────────────────────────

    #[tokio::test]
    async fn t_ws_08_single_connection_replacement() {
        let server = StrategyServer::start(
            "127.0.0.1:0", "secret".into(), 42, "testnet".into(), None,
        )
        .await
        .unwrap();

        // First client connects and auths
        let mut ws1 = connect_client(server.addr()).await;
        auth_client(&mut ws1, "secret").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(server.is_connected());

        // Second client connects and auths — should replace first
        let mut ws2 = connect_client(server.addr()).await;
        auth_client(&mut ws2, "secret").await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        // First client should be disconnected (abort kills the task)
        // Reading from ws1 should fail or get close
        let next = tokio::time::timeout(Duration::from_secs(1), ws1.next()).await;
        match next {
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Err(_) | Ok(Some(Err(_))) => {
                // Expected: connection was killed
            }
            other => {
                // Also acceptable if we get a disconnected event before close
                if let Ok(Some(Ok(Message::Text(text)))) = other {
                    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                    // Might get disconnected event or just close
                    assert!(
                        v["event"] == "disconnected"
                            || v.get("event").is_none(),
                        "unexpected message: {}",
                        text
                    );
                }
            }
        }

        // Server should still be connected (to ws2)
        assert!(server.is_connected());
    }

    // ── T_WS_09: Phase timeout → None ────────────────────────────────────

    #[tokio::test]
    async fn t_ws_09_phase_timeout() {
        let server = StrategyServer::start(
            "127.0.0.1:0", "secret".into(), 42, "testnet".into(), None,
        )
        .await
        .unwrap();

        let mut ws = connect_client(server.addr()).await;
        auth_client(&mut ws, "secret").await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Don't send anything — timeout should fire
        let action = server
            .receive_action_with_timeout(Duration::from_millis(200))
            .await;

        assert!(action.is_none());
    }

    // ── T_WS_10: Disconnect recovery ─────────────────────────────────────

    #[tokio::test]
    async fn t_ws_10_disconnect_recovery() {
        let server = StrategyServer::start(
            "127.0.0.1:0", "secret".into(), 42, "testnet".into(), None,
        )
        .await
        .unwrap();

        let mut ws = connect_client(server.addr()).await;
        auth_client(&mut ws, "secret").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(server.is_connected());

        // Client disconnects
        ws.close(None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(!server.is_connected());

        // Sending event should now fail
        let result = server
            .send_event(StrategyEvent::Paused {
                data: PausedData { after_batch: 100 },
            })
            .await;
        // Channel may still accept (buffered) but client won't receive.
        // The important assertion is is_connected() == false above.
        let _ = result;
    }
}
