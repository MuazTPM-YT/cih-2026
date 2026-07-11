#!/usr/bin/env bash
# PLACEHOLDER — owned by Muaz (muaz/core) per tasks/CONTRACTS.md.
# fmt + clippy(-D warnings) + test across the whole workspace.
set -euo pipefail

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
