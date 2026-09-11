#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 BINARY VERSION TARGET" >&2
  exit 2
fi

binary=$1
version=$2
target=$3
[[ "$version" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "invalid version" >&2; exit 2; }
[[ "$target" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "invalid target" >&2; exit 2; }
root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/dist"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir -p "$out"
if [[ "$target" == windows-* ]]; then
  install -Dm0755 "$binary" "$stage/bin/servoloop.exe"
else
  install -Dm0755 "$binary" "$stage/bin/servoloop"
fi
cp "$root/LICENSE" "$root/crates/servoloop-providers/NOTICE" "$stage/"
cat >"$stage/README" <<EOF
ServoLoop $version ($target)

Run ./bin/servoloop --help for usage. This archive contains the ServoLoop CLI,
licensed under AGPL-3.0-only; see LICENSE and NOTICE.
EOF

archive="$out/servoloop-${version}-${target}"
if [[ "$target" == windows-* ]]; then
  (cd "$stage" && zip -q -r "${archive}.zip" .)
  checksum_file="${archive}.zip"
else
  tar -C "$stage" -czf "${archive}.tar.gz" .
  checksum_file="${archive}.tar.gz"
fi
if command -v sha256sum >/dev/null; then
  (cd "$out" && sha256sum "$(basename "$checksum_file")" > SHA256SUMS)
else
  (cd "$out" && shasum -a 256 "$(basename "$checksum_file")" > SHA256SUMS)
fi
echo "$checksum_file"
