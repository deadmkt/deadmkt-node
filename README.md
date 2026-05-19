# deadmkt-node

Trading node for the DeadMKT decentralized batch-auction protocol on Supra.

## One-Liner Install (recommended)

On a fresh Linux VPS (Ubuntu/Debian/Fedora/RHEL/Rocky/AlmaLinux) with sudo:

```bash
curl -sSL https://get.deadmkt.com | bash
```

The installer checks prerequisites, installs Docker + git if missing, clones this repo, builds the image, runs the non-interactive setup wizard (asks for your beneficiary address + a keystore password), starts the node with `--restart=unless-stopped`, and prints the status JSON. About 5-8 minutes on a small VPS, mostly `docker build` time.

Idempotent: re-running skips prompts when a keystore already exists; passes `--rebuild` to force a fresh image build.

The installer is just bash -- read it before piping into your shell: [`install.sh`](install.sh).

## Manual install

```bash
git clone https://github.com/deadmkt/deadmkt-node.git
cd deadmkt-node
docker build -t deadmkt-node .
docker run -it -v ~/.deadmkt:/data deadmkt-node deadmkt-node setup
```

The setup wizard guides you through keypair generation, funding, token minting, and escrow registration. After setup:

```bash
docker run -d --name deadmkt-node \
  -v ~/.deadmkt:/data \
  -p 127.0.0.1:9090:9090 \
  -p 127.0.0.1:9292:9292 \
  -p 9191:9191 \
  --restart unless-stopped \
  deadmkt-node
```

Non-interactive setup (for scripting / LLM-assisted operators): see `examples/setup-config*.json` and run `deadmkt-node --config /data/setup.json`.

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
