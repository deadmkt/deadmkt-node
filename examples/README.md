# deadmkt-node examples

## Non-interactive setup configs (MR1)

Three flavours of `setup.json`, consumed via:

```bash
deadmkt-node --config setup.json
```

| File | Use when |
|------|----------|
| `setup-config.json` | Fresh setup, automated/script context. Includes `keystore_password`. |
| `setup-config-llm.json` | LLM-assisted: an agent wrote the config, the human types the password at the prompt (no secret leaves the chat). |
| `setup-config-bootstrap.json` | Bootstrap-only node (gossip relay, no trading). |

### Security notes

- A config carrying `keystore_password` **must be `chmod 600`** -- the node refuses to read it otherwise (MR1d). The check mirrors SSH's stance on private keys.
- After a successful fresh setup, the node **scrubs `keystore_password` from the file** so a stale on-disk copy doesn't outlive its single use.
- The LLM-assisted variant has no password field at all; the node prompts on stderr at startup.

### Release builds

`--features production` on the keystore crate disables the insecure (unencrypted) keystore path entirely. Scale-test and dev builds leave it available.

## ws_demo.rs

Tiny example showing how to connect a strategy over the WebSocket bridge.
