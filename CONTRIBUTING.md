# Contributing

Thanks for improving `oremap`.

## Local Setup

```bash
./scripts/setup.sh
```

## Development Checks

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo run -- scan --systemd-scope off --no-cron --repo-root . --format json
```

## Standards

- Keep collectors read-only.
- Run external tools with `std::process::Command` and argument vectors, never shell-form commands, and only from the reviewed absolute-path allowlist.
- Do not emit secret values from env files or cron entries.
- Keep command-line behavior and report output stable; the golden help text in `tests/golden/` and the differential suite (`tests/differential.rs`) guard it.
- Add focused tests for new parsing or classification behavior.
- Keep runtime dependencies minimal and pinned to exact versions; add one only when it removes meaningful risk or code.

## Pull Requests

Open a PR with:

- a short summary of behavior changed
- validation commands run
- any privacy or security tradeoffs introduced
