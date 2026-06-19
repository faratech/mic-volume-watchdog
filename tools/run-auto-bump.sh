#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if ! command -v cargo-auto-bump >/dev/null 2>&1; then
  echo "cargo-auto-bump is not installed." >&2
  echo "Install once with: cargo install --path \"$SCRIPT_DIR/cargo-auto-bump\" --force --locked" >&2
  echo "Then rerun this command." >&2
  exit 1
fi

exec cargo-auto-bump "$@"
