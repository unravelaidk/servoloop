#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/archive dir/bin" "$tmp/prefix dir"
printf '#!/bin/sh\necho fixture\n' > "$tmp/archive dir/bin/servoloop"
chmod +x "$tmp/archive dir/bin/servoloop"
printf 'fixture license\n' > "$tmp/archive dir/LICENSE"
printf 'fixture notice\n' > "$tmp/archive dir/NOTICE"
printf 'fixture readme\n' > "$tmp/archive dir/README"
(cd "$tmp/archive dir" && tar -czf "$tmp/fixture.tar.gz" .)
(cd "$tmp" && sha256sum fixture.tar.gz > SHA256SUMS)
bash "$root/scripts/install.sh" --archive "$tmp/fixture.tar.gz" \
  --checksum-file "$tmp/SHA256SUMS" --prefix "$tmp/prefix dir"
test -x "$tmp/prefix dir/servoloop"
before=$(sha256sum "$tmp/prefix dir/servoloop")
printf 'bad\n' > "$tmp/SHA256SUMS"
if bash "$root/scripts/install.sh" --archive "$tmp/fixture.tar.gz" \
  --checksum-file "$tmp/SHA256SUMS" --prefix "$tmp/prefix dir"; then
  echo 'bad checksum unexpectedly succeeded' >&2
  exit 1
fi
test "$before" = "$(sha256sum "$tmp/prefix dir/servoloop")"
echo 'installer tests passed'
