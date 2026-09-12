#!/usr/bin/env bash
set -euo pipefail

repository="lucaswilliameufrasio/device-simulator-mcp"
tag="latest"
bin_dir="${HOME}/.local/bin"

usage() {
  printf '%s\n' \
    'Usage: install-private.sh [--tag vX.Y.Z] [--bin-dir PATH]' \
    'Requires an authenticated gh CLI session.'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)
      tag="$2"
      shift 2
      ;;
    --bin-dir)
      bin_dir="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'Unknown argument: %s\n' "$1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

command -v gh >/dev/null 2>&1 || {
  printf '%s\n' 'gh CLI is required.' >&2
  exit 1
}

os="$(uname -s)"
architecture="$(uname -m)"
case "$os/$architecture" in
  Darwin/arm64|Darwin/aarch64) target="aarch64-apple-darwin" ;;
  Darwin/x86_64|Darwin/amd64) target="x86_64-apple-darwin" ;;
  Linux/arm64|Linux/aarch64) target="aarch64-unknown-linux-gnu" ;;
  Linux/x86_64|Linux/amd64) target="x86_64-unknown-linux-gnu" ;;
  *)
    printf 'Unsupported host: %s/%s\n' "$os" "$architecture" >&2
    exit 1
    ;;
esac

asset="device-simulator-mcp-${target}.tar.xz"
temporary_directory="$(mktemp -d)"
trap 'rm -rf "$temporary_directory"' EXIT

download_arguments=(release download)
if [[ "$tag" != "latest" ]]; then
  download_arguments+=("$tag")
fi
download_arguments+=(
  --repo "$repository"
  --pattern "$asset"
  --pattern "${asset}.sha256"
  --dir "$temporary_directory"
)
gh "${download_arguments[@]}"

expected_hash="$(awk '{print $1}' "$temporary_directory/${asset}.sha256")"
actual_hash="$(shasum -a 256 "$temporary_directory/$asset" | awk '{print $1}')"
if [[ "$expected_hash" != "$actual_hash" ]]; then
  printf 'Checksum mismatch for %s\n' "$asset" >&2
  exit 1
fi

mkdir -p "$bin_dir"
tar --strip-components=1 -xf "$temporary_directory/$asset" -C "$temporary_directory"
install -m 0755 "$temporary_directory/device-simulator-mcp" \
  "$bin_dir/device-simulator-mcp"
printf 'Installed device-simulator-mcp to %s\n' "$bin_dir/device-simulator-mcp"
