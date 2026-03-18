"""
strategy_wrapper/bridge.py
Bridge between /data/strategy.py and ws://localhost:9090.

- Connects to node WS server
- Authenticates with token
- Routes events to strategy functions
- File-watch for hot reload at batch boundaries
- B5_9.1: Supports token actions (mint/claim_mint) from on_batch_start

Usage:
    DEADMKT_AUTH_TOKEN=<token> python3 bridge.py [--strategy /data/strategy.py]
"""

import asyncio
import importlib.util
import json
import os
import sys
import time
import logging

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [strategy] %(message)s",
)
log = logging.getLogger("bridge")

DEFAULT_STRATEGY_PATH = "/data/strategy.py"
DEFAULT_WS_URL = "ws://localhost:9090"


def load_strategy(path):
    """Load strategy module from file path."""
    spec = importlib.util.spec_from_file_location("strategy", path)
    if spec is None:
        raise ImportError(f"Cannot load strategy from {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class StrategyBridge:
    """Manages WS connection and strategy lifecycle."""

    def __init__(self, ws_url, auth_token, strategy_path):
        self.ws_url = ws_url
        self.auth_token = auth_token
        self.strategy_path = strategy_path
        self.strategy = None
        self.last_mtime = 0
        self.reload_pending = False

    def load(self):
        """Load or reload strategy module."""
        try:
            self.strategy = load_strategy(self.strategy_path)
            self.last_mtime = os.path.getmtime(self.strategy_path)
            log.info(f"Strategy loaded from {self.strategy_path}")
        except Exception as e:
            log.error(f"Failed to load strategy: {e}")
            self.strategy = None

    def check_reload(self):
        """Check if strategy file changed. If so, mark for reload at next batch."""
        try:
            mtime = os.path.getmtime(self.strategy_path)
            if mtime > self.last_mtime:
                self.reload_pending = True
                log.info("Strategy file changed — will reload at next batch boundary")
        except OSError:
            pass

    def do_reload(self):
        """Actually reload the strategy (called at batch boundary)."""
        if self.reload_pending:
            log.info("Reloading strategy...")
            self.load()
            self.reload_pending = False

    async def handle_event(self, event_data):
        """Route a node event to the appropriate strategy function.

        Returns a list of action dicts to send to the node.
        Most events return 0 or 1 actions. batch_start can return
        a commit + token actions (e.g. mint, claim_mint).
        """
        event = event_data.get("event", "")
        data = event_data.get("data", {})

        if event == "batch_start":
            # Check for hot reload at batch boundary
            self.do_reload()

            if self.strategy and hasattr(self.strategy, "on_batch_start"):
                try:
                    result = self.strategy.on_batch_start(data)
                    return self._parse_batch_result(result)
                except Exception as e:
                    log.error(f"on_batch_start error: {e}")
                    return [{"action": "commit", "orders": []}]
            else:
                return [{"action": "commit", "orders": []}]

        elif event == "reveal_start":
            my_commits = data.get("my_commits", [])
            if self.strategy and hasattr(self.strategy, "on_reveal"):
                try:
                    indices = self.strategy.on_reveal(data, my_commits)
                    return [{"action": "reveal", "reveal_indices": indices or []}]
                except Exception as e:
                    log.error(f"on_reveal error: {e}")
                    return [{"action": "reveal", "reveal_indices": list(range(len(my_commits)))}]

        elif event == "match_result":
            if self.strategy and hasattr(self.strategy, "on_match"):
                try:
                    self.strategy.on_match(data, data.get("matches", []))
                except Exception as e:
                    log.error(f"on_match error: {e}")

        elif event == "settlement":
            if self.strategy and hasattr(self.strategy, "on_settlement"):
                try:
                    self.strategy.on_settlement(data, data)
                except Exception as e:
                    log.error(f"on_settlement error: {e}")

        elif event == "token_action_result":
            if self.strategy and hasattr(self.strategy, "on_token_result"):
                try:
                    self.strategy.on_token_result(data)
                except Exception as e:
                    log.error(f"on_token_result error: {e}")

        elif event == "auth_ok":
            log.info(f"Authenticated. NFT ID: {data.get('nft_id')}, Network: {data.get('network')}")
            # B5_9.1: Fire startup actions immediately after auth
            # (before commit timeout can kill the connection)
            if self.strategy and hasattr(self.strategy, "on_auth"):
                try:
                    startup_actions = self.strategy.on_auth(data)
                    if startup_actions and isinstance(startup_actions, list):
                        result = []
                        for sa in startup_actions:
                            if isinstance(sa, dict) and "action" in sa:
                                result.append(sa)
                                log.info(f"Startup action queued: {sa.get('action')}")
                        if result:
                            return result
                except Exception as e:
                    log.error(f"on_auth error: {e}")

        elif event == "auth_failed":
            log.error(f"Auth failed: {data.get('reason')}")
            return ["__auth_failed__"]

        elif event == "disconnected":
            log.warning(f"Disconnected: {data.get('reason')}")

        else:
            log.debug(f"Unhandled event: {event}")

        return []

    def _parse_batch_result(self, result):
        """Parse on_batch_start return value into a list of WS actions.

        Supports three return formats:
        1. list of orders (backward compatible):
              [{"pair": ..., "side": ..., "price": ..., "quantity": ...}]
              → sends: {"action": "commit", "orders": [...]}

        2. dict with orders + token_actions:
              {"orders": [...], "token_actions": [{"action": "mint", ...}, ...]}
              → sends: {"action": "commit", ...} then each token action

        3. None → empty commit
        """
        if result is None:
            return [{"action": "commit", "orders": []}]

        if isinstance(result, list):
            # Backward compatible: list of orders
            return [{"action": "commit", "orders": result}]

        if isinstance(result, dict):
            actions = []
            # Commit orders (may be empty)
            orders = result.get("orders", [])
            actions.append({"action": "commit", "orders": orders})

            # Token actions (mint, claim_mint, burn, lock, unlock)
            for ta in result.get("token_actions", []):
                if isinstance(ta, dict) and "action" in ta:
                    actions.append(ta)
                    log.info(f"Token action queued: {ta.get('action')}")

            return actions

        # Unknown format — treat as empty
        log.warning(f"Unknown on_batch_start return type: {type(result)}")
        return [{"action": "commit", "orders": []}]

    async def run(self):
        """Main loop: connect, auth, handle events."""
        try:
            import websockets
        except ImportError:
            log.error("websockets package not installed. Run: pip install websockets")
            return

        self.load()

        while True:
            try:
                log.info(f"Connecting to {self.ws_url}...")
                async with websockets.connect(
                    self.ws_url,
                    ping_interval=30,
                    ping_timeout=60,
                    close_timeout=10,
                ) as ws:
                    # Auth
                    auth_msg = json.dumps({"action": "auth", "token": self.auth_token})
                    await ws.send(auth_msg)

                    # Main event loop
                    async for raw in ws:
                        try:
                            event_data = json.loads(raw)
                        except json.JSONDecodeError:
                            continue

                        responses = await self.handle_event(event_data)
                        for response in responses:
                            if response == "__auth_failed__":
                                log.info("Retrying auth in 10 seconds...")
                                await asyncio.sleep(10)
                                break
                            if response:
                                await ws.send(json.dumps(response))

                        # Check for file changes periodically
                        self.check_reload()

            except Exception as e:
                log.error(f"Connection error: {e}")
                log.info("Reconnecting in 5 seconds...")
                await asyncio.sleep(5)


def main():
    strategy_path = os.environ.get("DEADMKT_STRATEGY_PATH", DEFAULT_STRATEGY_PATH)
    ws_url = os.environ.get("DEADMKT_WS_URL", DEFAULT_WS_URL)
    auth_token = os.environ.get("DEADMKT_AUTH_TOKEN", "")

    if not auth_token:
        log.error("DEADMKT_AUTH_TOKEN not set")
        sys.exit(1)

    if not os.path.exists(strategy_path):
        log.error(f"Strategy file not found: {strategy_path}")
        sys.exit(1)

    bridge = StrategyBridge(ws_url, auth_token, strategy_path)
    asyncio.run(bridge.run())


if __name__ == "__main__":
    main()
