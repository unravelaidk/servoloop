#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 || ! -f "$1" ]]; then
  echo "usage: $0 PATH-TO-BUILT-SERVOLOOP" >&2
  exit 2
fi
root=$(cd "$(dirname "$0")/.." && pwd)
exec node "$root/scripts/test-npm-install.mjs" "$1" 0.1.0 linux-x64-glibc-2.39
