#!/bin/bash
# =========================================================================
# entrypoint.sh: Process manager for deadmkt-node
# =========================================================================
#
# If CLI args passed (e.g. "deadmkt-node setup") → exec directly.
# If no args → full startup: node + strategy wrapper.
#
# Environment variables:
#   DEADMKT_NO_STRATEGY=1     Skip strategy wrapper entirely (peer-only mode)
#   DEADMKT_AUTH_TOKEN=<tok>   Override auth token (otherwise read from config)
#   DEADMKT_KEYSTORE_PASSWORD  Keystore password for node signing

set -e

# ── If arguments passed, exec them directly ───────────────────────────
if [ $# -gt 0 ]; then
    exec "$@"
fi

# ── Full startup (default: no args) ──────────────────────────────────

DATA_DIR="${DEADMKT_DATA_DIR:-/data}"

# First boot: run setup wizard interactively before anything else
if [ ! -f "$DATA_DIR/config.json" ] || [ ! -f "$DATA_DIR/keystore.json" ]; then
    echo "[entrypoint] First boot detected — running setup wizard..."
    if ! deadmkt-node setup; then
        echo "[entrypoint] Setup failed. Exiting."
        exit 1
    fi
    echo "[entrypoint] Setup complete. Starting node..."
fi

# Read auth token from config.json if not overridden
AUTH_TOKEN="${DEADMKT_AUTH_TOKEN:-}"
if [ -z "$AUTH_TOKEN" ] && [ -f "$DATA_DIR/config.json" ]; then
    AUTH_TOKEN=$(grep -o '"strategy_auth_token"[[:space:]]*:[[:space:]]*"[^"]*"' "$DATA_DIR/config.json" | sed 's/.*: *"//;s/"//' 2>/dev/null || true)
fi
if [ -z "$AUTH_TOKEN" ]; then
    AUTH_TOKEN=$(head -c 32 /dev/urandom | base64 | tr -d '=+/' | head -c 32)
fi
export DEADMKT_AUTH_TOKEN="$AUTH_TOKEN"

# Start Rust node
echo "[entrypoint] Starting deadmkt-node..."
deadmkt-node &
NODE_PID=$!
WRAPPER_PID=""

# Skip strategy wrapper if DEADMKT_NO_STRATEGY is set
if [ "${DEADMKT_NO_STRATEGY:-}" = "1" ]; then
    echo "[entrypoint] No strategy mode — node only (peer/bootstrap)"
else
    STRATEGY_FILE="${DEADMKT_STRATEGY_PATH:-/data/strategy.py}"

    # Copy starter bot on first boot only — operators replace with their own
    if [ ! -f "$STRATEGY_FILE" ]; then
        echo "[entrypoint] No strategy found. Installing starter bot..."
        cp /opt/deadmkt/starter_bot/strategy.py "$STRATEGY_FILE"
        echo "[entrypoint] Replace $STRATEGY_FILE with your own strategy."
    fi

    # Wait for node WebSocket to be ready
    echo "[entrypoint] Waiting for node WebSocket..."
    for i in $(seq 1 30); do
        if nc -z localhost 9090 2>/dev/null; then
            echo "[entrypoint] WebSocket ready after ${i}s"
            break
        fi
        sleep 1
    done

    # Start Python strategy wrapper
    if [ -f "$STRATEGY_FILE" ]; then
        echo "[entrypoint] Starting strategy wrapper..."
        export DEADMKT_STRATEGY_PATH="$STRATEGY_FILE"
        python3 /opt/deadmkt/strategy_wrapper/bridge.py &
        WRAPPER_PID=$!
    fi
fi

# Wait for signals
trap 'echo "[entrypoint] Shutting down..."; kill $NODE_PID $WRAPPER_PID 2>/dev/null; exit 0' SIGTERM SIGINT

if [ -n "$WRAPPER_PID" ]; then
    wait -n $NODE_PID $WRAPPER_PID 2>/dev/null || true
else
    wait $NODE_PID 2>/dev/null || true
fi
echo "[entrypoint] Process exited. Shutting down..."
kill $NODE_PID $WRAPPER_PID 2>/dev/null || true
wait
