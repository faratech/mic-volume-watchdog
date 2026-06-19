#!/usr/bin/env bash
set -euo pipefail

SCRIPT_LINUX_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/host_mic_optimization_test.ps1"
SCRIPT_WINDOWS_PATH="$(wslpath -w "$SCRIPT_LINUX_PATH")"

if ! command -v powershell.exe >/dev/null 2>&1; then
  echo "powershell.exe not found. Run this on a WSL session with Windows host access."
  exit 1
fi

args=("-NoProfile" "-ExecutionPolicy" "Bypass" "-File" "$SCRIPT_WINDOWS_PATH")
args+=("$@")

exec powershell.exe "${args[@]}"
