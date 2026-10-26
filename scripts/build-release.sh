#!/usr/bin/env bash
# Build the release binaries and a versioned, checksummed archive for the
# host target:
#
#   dist/oremap-v<version>-<target>.tar.gz
#   dist/oremap-v<version>-<target>.tar.gz.sha256
#
# The "v" matches the tag and the other OpenForge release assets
# (releases up to 0.4.3 were named oremap-<version>-<target>).
#
# The archive is reproducible for a given commit and toolchain: entries are
# sorted, owned by 0:0, and stamped with SOURCE_DATE_EPOCH (default: the
# last commit time). Pass cargo flags such as `-j 3` as arguments.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)"
TARGET="$(rustc -vV | sed -n 's/^host: //p')"
if [ -z "$VERSION" ] || [ -z "$TARGET" ]; then
    echo "ERROR: could not read the package version or host target." >&2
    exit 1
fi
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || echo 0)}"
export SOURCE_DATE_EPOCH

# Build paths (the project and cargo's registry under the builder's home)
# would otherwise be embedded in panic locations: remap them so the binary
# depends only on the commit and toolchain, and names no local user.
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$CARGO_HOME_DIR=/cargo"$'\x1f'"--remap-path-prefix=$PROJECT_DIR=/oremap"
export CARGO_ENCODED_RUSTFLAGS

cargo build --release --locked "$@"

for binary in target/release/oremap target/release/service-cartographer; do
    for prefix in "$CARGO_HOME_DIR" "$PROJECT_DIR"; do
        if grep -qaF -- "$prefix" "$binary"; then
            echo "ERROR: $binary embeds the build path $prefix." >&2
            exit 1
        fi
    done
done

reported="$(target/release/oremap --version)"
if [ "$reported" != "oremap $VERSION" ]; then
    echo "ERROR: built binary reports '$reported', expected 'oremap $VERSION'." >&2
    exit 1
fi

NAME="oremap-v$VERSION-$TARGET"
STAGE="dist/$NAME"
rm -rf "$STAGE" "dist/$NAME.tar.gz" "dist/$NAME.tar.gz.sha256"
mkdir -p "$STAGE"
chmod 0755 "$STAGE"
# The deprecated `service-cartographer` alias ships while Cargo.toml builds it.
install -m 0755 target/release/oremap target/release/service-cartographer "$STAGE/"
install -m 0644 README.md CHANGELOG.md LICENSE "$STAGE/"

tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
    -C dist -cf - "$NAME" | gzip -n -9 > "dist/$NAME.tar.gz"
rm -rf "$STAGE"
(cd dist && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")

echo "Built dist/$NAME.tar.gz"
cat "dist/$NAME.tar.gz.sha256"
