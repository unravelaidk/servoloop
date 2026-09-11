#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage: install.sh [--version VERSION] [--prefix DIR]
       install.sh --archive FILE --checksum-file SHA256SUMS [--prefix DIR]

Download mode uses SERVOLOOP_RELEASE_URL (an HTTPS archive base URL).
The installer never changes shell startup files; add the prefix to PATH yourself.
EOF
}
version=""
prefix="${SERVOLOOP_INSTALL_PREFIX:-$HOME/.local/bin}"
archive=""
checksums=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) [[ $# -ge 2 ]] || { usage; exit 2; }; version=$2; shift 2 ;;
    --prefix) [[ $# -ge 2 ]] || { usage; exit 2; }; prefix=$2; shift 2 ;;
    --archive) [[ $# -ge 2 ]] || { usage; exit 2; }; archive=$2; shift 2 ;;
    --checksum-file) [[ $# -ge 2 ]] || { usage; exit 2; }; checksums=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage; exit 2 ;;
  esac
done

os=$(uname -s); arch=$(uname -m)
case "$os:$arch" in
  Linux:x86_64)
    target=linux-x86_64-glibc-2.39; extension=tar.gz
    glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')
    if [[ -z "$glibc" ]] || [[ "$(printf '%s\n' 2.39 "$glibc" | sort -V | head -n1)" != "2.39" ]]; then
      echo "Linux archive requires glibc 2.39 or newer (detected: ${glibc:-unknown}); use a newer host or build from source" >&2
      exit 1
    fi
    ;;
  Darwin:arm64) target=macos-arm64; extension=tar.gz ;;
  Darwin:x86_64) target=macos-x86_64; extension=tar.gz ;;
  *) echo "unsupported platform: $os $arch (supported: Linux x86_64, macOS arm64/x86_64)" >&2; exit 1 ;;
esac
if [[ -n "$archive" && -z "$checksums" || -z "$archive" && -n "$checksums" ]]; then
  echo "--archive and --checksum-file must be provided together" >&2; exit 2
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [[ -z "$archive" ]]; then
  [[ -n "$version" ]] || { echo "--version is required for download mode" >&2; exit 2; }
  base="${SERVOLOOP_RELEASE_URL:-}"
  [[ -n "$base" ]] || { echo "set SERVOLOOP_RELEASE_URL or use --archive" >&2; exit 2; }
  name="servoloop-${version}-${target}.${extension}"
  archive="$tmp/$name"
  checksums="$tmp/SHA256SUMS"
  curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 \
    "${base%/}/v${version}/$name" --output "$archive" \
    --proto-redir '=https'
  curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 \
    "${base%/}/v${version}/SHA256SUMS" --output "$checksums" \
    --proto-redir '=https'
fi
[[ -f "$archive" && -f "$checksums" ]] || { echo "archive or checksum file not found" >&2; exit 1; }
expected=$(awk -v n="$(basename "$archive")" '$2 == n { print $1; exit }' "$checksums")
[[ "$expected" =~ ^[[:xdigit:]]{64}$ ]] || { echo "no SHA-256 entry for $(basename "$archive")" >&2; exit 1; }
actual=$(sha256sum "$archive" 2>/dev/null | awk '{print $1}' || shasum -a 256 "$archive" | awk '{print $1}')
[[ "$actual" == "$expected" ]] || { echo "checksum verification failed" >&2; exit 1; }
mkdir -p "$tmp/unpack"
case "$archive" in
  *.tar.gz|*.tgz)
    entries=$(tar -tzf "$archive")
    while IFS= read -r entry; do
      [[ "$entry" == ./ || "$entry" == ./bin/ || "$entry" == ./bin/servoloop || "$entry" == ./LICENSE || "$entry" == ./NOTICE || "$entry" == ./README ]] || { echo "archive contains unexpected path: $entry" >&2; exit 1; }
    done <<<"$entries"
    tar -tvzf "$archive" | awk 'substr($0,1,1) != "-" && substr($0,1,1) != "d" { exit 1 }' || { echo "archive contains a link or special file" >&2; exit 1; }
    tar -xzf "$archive" -C "$tmp/unpack" ;;
  *.zip)
    command -v unzip >/dev/null || { echo "unzip is required" >&2; exit 1; }
    command -v zipinfo >/dev/null || { echo "zipinfo is required" >&2; exit 1; }
    entries=$(unzip -Z1 "$archive")
    while IFS= read -r entry; do
      [[ "$entry" == bin/ || "$entry" == bin/servoloop.exe || "$entry" == LICENSE || "$entry" == NOTICE || "$entry" == README ]] || { echo "archive contains unexpected path: $entry" >&2; exit 1; }
    done <<<"$entries"
    zipinfo -l "$archive" | awk 'NR > 3 && /^[[:space:]]*[lh]/ { exit 1 }' || { echo "archive contains a link or special file" >&2; exit 1; }
    unzip -q "$archive" -d "$tmp/unpack" ;;
  *) echo "unsupported archive format" >&2; exit 1 ;;
esac
[[ -f "$tmp/unpack/bin/servoloop" ]] || { echo "archive does not contain bin/servoloop" >&2; exit 1; }
chmod 0755 "$tmp/unpack/bin/servoloop"
mkdir -p "$prefix"
staged="$prefix/.servoloop.tmp.$$"
install -m 0755 "$tmp/unpack/bin/servoloop" "$staged"
mv -f "$staged" "$prefix/servoloop"
echo "installed $prefix/servoloop" >&2
echo "Add $prefix to PATH if it is not already present." >&2
