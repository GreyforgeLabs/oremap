# STARTHERE.md - Coding Assistant Bootstrap Guide

> This file is designed for coding assistants. If you are a human, see [README.md](README.md).

## Quick Bootstrap

```bash
git clone https://github.com/GreyforgeLabs/oremap.git && cd oremap && ./scripts/setup.sh
```

## What This Project Does

`oremap` is a read-only CLI that inventories local workflow surfaces: systemd units, cron jobs, wrapper scripts, Git repositories, and environment files. It emits JSON, Markdown, or CSV so maintainers can make explicit keep, review, or retire decisions.

Version 0.4.0 is a Rust rewrite of the 0.3.x Python implementation with identical command-line behavior and report output, and the first release named `oremap` (formerly `service-cartographer`). Only the program name and the Markdown report title changed with the rename; the JSON schema identifier stays `service-cartographer.report.v1` for existing consumers. A deprecated `service-cartographer` alias binary (`src/bin/service-cartographer.rs`) prints a one-line notice to stderr and then runs `oremap` unchanged; it is removed in the next release. Differences from the Python version are listed in [CHANGELOG.md](CHANGELOG.md).

## Project Structure

```text
src/
  main.rs         binary entry point (`oremap`)
  bin/service-cartographer.rs  deprecated alias binary (one release)
  cli.rs          scan orchestration, report assembly, private report writes
  argparse.rs     argparse-compatible parser and help formatter
  collectors.rs   read-only systemd, cron, wrapper, Git, and env-file collectors
  classifier.rs   keep/review/retire hint logic
  formatters.rs   JSON, Markdown, and CSV renderers
  models.rs       inventory item/report types and the JSON writer
  privacy.rs      home-path and credential redaction, CSV formula guard
  paths.rs        pathlib-compatible path spelling and ~ expansion
  pyfmt.rs        Python-compatible string helpers (whitespace, repr, numbers)
  timefmt.rs      ISO-8601 timestamp formatting and parsing
tests/
  cli.rs          end-to-end binary tests
  differential.rs comparison against the Python reference (opt-in)
  golden/         reference help and error text
scripts/setup.sh  idempotent setup and verification
```

## Setup Prerequisites

- Rust 1.88 or newer with `rustfmt` and `clippy`
- `git` for repository scanning
- Linux with `systemctl` and `crontab` for full host scans

## Installation Steps

```bash
cargo build --locked
cargo install --path .
```

## Verification

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo run -- scan --systemd-scope off --no-cron --repo-root . --format json
```

To run the differential suite against the last Python release:

```bash
git worktree add --detach ../oremap-python v0.3.0   # or any Python-era commit
OREMAP_PY_REF=../oremap-python/src cargo test --test differential
```

The reference still ships as the `service_cartographer` Python module. The
suite runs it with the argparse program name set to `oremap`, and the legacy
`SERVICE_CARTOGRAPHER_PY_REF` variable is still accepted.

## Key Entry Points

- CLI: `oremap scan`
- Library entry point: `oremap::cli::run`
- Collector functions: `oremap::collectors`

## Configuration

The tool is CLI-configured. Use `--repo-root`, `--wrapper-dir`, `--env-root`, `--unit-dir`, and `--focus` to scope scans. It does not require secrets or environment variables. `COLUMNS` controls help-text wrapping, as with Python's argparse.

## Common Tasks

```bash
# Run tests
cargo test --locked

# Lint
cargo clippy --all-targets --all-features -- -D warnings

# Build a release binary
cargo build --release --locked

# Local Markdown report
cargo run --release -- scan --format markdown --output report.md
```
