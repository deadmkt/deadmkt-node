# deadmkt-node

Trading node for the DeadMKT decentralized batch-auction protocol on Supra.

## Quick Start

```bash
git clone https://github.com/deadmkt/deadmkt-node.git
cd deadmkt-node
docker build -t deadmkt-node .
docker run -it -v deadmkt-data:/data deadmkt-node deadmkt-node setup
```

The setup wizard guides you through keypair generation, funding, token minting, and escrow registration.

After setup:

```bash
docker run -d --name deadmkt-node \
  -v deadmkt-data:/data \
  -e DEADMKT_KEYSTORE_PASSWORD='your_password' \
  --restart unless-stopped \
  deadmkt-node
```

## Build from Source

```bash
cargo build --release
cargo test
```

## Documentation

Full guides, strategy API reference, and protocol docs: [deadmkt.com/docs](https://deadmkt.com/docs/)

- [Run a Node](https://deadmkt.com/docs/guides/run-a-node)
- [Write a Strategy](https://deadmkt.com/docs/guides/write-a-strategy)
- [WebSocket API](https://deadmkt.com/docs/guides/websocket-api)
- [Token Actions](https://deadmkt.com/docs/guides/token-actions)

## License

The node infrastructure is licensed under [AGPL-3.0](LICENSE).

Example strategies (`starter_bot/` and `strategies/`) are licensed under [MIT](starter_bot/LICENSE).

See [deadmkt.com/docs/community/license](https://deadmkt.com/docs/community/license) for details.
