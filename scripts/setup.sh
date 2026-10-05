#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

echo "=== oremap Setup ==="

check_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "ERROR: $1 is required but not installed."
        exit 1
    fi
}

check_command cargo
check_command rustc

if ! cargo fmt --version >/dev/null 2>&1; then
    echo "ERROR: rustfmt is required. Install it with: rustup component add rustfmt"
    exit 1
fi

cd "$PROJECT_DIR"

echo "Building..."
cargo build --locked

echo "Running tests..."
cargo test --locked

echo "=== Setup complete ==="
echo "Running verification..."
REPORT="$(cargo run --quiet --locked -- scan \
    --systemd-scope off \
    --no-cron \
    --repo-root "$PROJECT_DIR" \
    --wrapper-dir "$PROJECT_DIR/scripts" \
    --format json)"
TOTAL="$(printf '%s\n' "$REPORT" | sed -n 's/^    "total": \([0-9][0-9]*\).*/\1/p')"
echo "Scanned ${TOTAL:-0} items"
