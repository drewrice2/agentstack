# Contributing

Follow [`AGENTS.md`](AGENTS.md).

Keep changes small. Match live `agentstack --help` and
[`docs/COMMANDS.md`](docs/COMMANDS.md) before editing examples.

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny --all-features check advisories bans licenses sources
```

Do not commit secrets (`.env`, `credentials.json`, tokens, keyring exports).
