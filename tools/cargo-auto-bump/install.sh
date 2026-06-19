#!/usr/bin/env bash
set -euo pipefail

MANIFEST_DIR="$(cd "$(dirname "$0")" && pwd)"

cargo install --path "$MANIFEST_DIR" --force --locked
