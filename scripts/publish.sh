#!/usr/bin/env bash
# Publish the crate to crates.io. Usage: scripts/publish.sh [--dry-run]
set -euo pipefail

cd "$(dirname "$0")/.."

version=$(cargo metadata --no-deps --format-version 1 |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')
tag="v$version"

if [ -n "$(git status --porcelain -- src examples docs README.md CHANGELOG.md LICENSE Cargo.toml Cargo.lock)" ]; then
  echo "error: uncommitted changes in packaged files" >&2
  exit 1
fi

if [ "$(git rev-parse -q --verify "refs/tags/$tag^{commit}" || true)" != "$(git rev-parse HEAD)" ]; then
  echo "error: tag $tag does not point at HEAD" >&2
  exit 1
fi

cargo clippy --all-targets --release -- -D warnings
cargo test --release --doc

if [ "${1:-}" = "--dry-run" ]; then
  cargo publish --dry-run
else
  cargo publish
  git push origin "$tag"
fi
