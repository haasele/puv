#!/bin/sh
# Install the latest PUV release from github.com/haasele/puv.
# Usage: curl -fsSL https://raw.githubusercontent.com/haasele/puv/main/install.sh | sh
set -eu

REPO="haasele/puv"
DEST="${PUV_INSTALL_DIR:-${HOME}/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf '%s\n' "$*" >&2; exit 1; }

os=$(uname -s)
machine=$(uname -m)
case "$machine" in
  x86_64 | amd64) arch="x86_64" ;;
  aarch64 | arm64) arch="aarch64" ;;
  *) die "Unsupported architecture: ${machine}" ;;
esac

case "$os" in
  Linux)
    target="${arch}-unknown-linux-gnu"
    bin_name="puv"
    ;;
  Darwin)
    target="${arch}-apple-darwin"
    bin_name="puv"
    ;;
  MINGW* | MSYS* | CYGWIN*)
    target="${arch}-pc-windows-msvc"
    bin_name="puv.exe"
    ;;
  *)
    die "No published build for ${os}. Build from source: git clone https://github.com/${REPO} && cargo install --path ${REPO##*/}/crates/puv --locked"
    ;;
esac

asset="puv-${target}.tar.gz"
url="https://github.com/${REPO}/releases/latest/download/${asset}"
sums_url="https://github.com/${REPO}/releases/latest/download/sha256sums.txt"

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

say "Fetching the latest release asset: ${asset}"
if ! curl -fL --retry 3 --retry-delay 1 -o "${tmp}/asset.tar.gz" "$url"; then
  die "No ${asset} on the latest GitHub release of ${REPO}. Build from source: git clone https://github.com/${REPO} && cargo install --path ${REPO##*/}/crates/puv --locked"
fi

if curl -fsSL -o "${tmp}/sha256sums.txt" "$sums_url"; then
  expected=$(awk -v name="$asset" '$2 == name { print $1; exit }' "${tmp}/sha256sums.txt")
  if [ -n "${expected}" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
      actual=$(sha256sum "${tmp}/asset.tar.gz" | awk '{ print $1 }')
    elif command -v shasum >/dev/null 2>&1; then
      actual=$(shasum -a 256 "${tmp}/asset.tar.gz" | awk '{ print $1 }')
    else
      actual=""
    fi
    if [ -n "${actual}" ] && [ "${actual}" != "${expected}" ]; then
      die "Checksum mismatch for ${asset}"
    fi
    if [ -n "${actual}" ]; then
      say "Checksum matched sha256sums.txt"
    fi
  fi
fi

tar -xzf "${tmp}/asset.tar.gz" -C "$tmp"
bin=$(find "$tmp" -type f -name "$bin_name" | head -n 1)
[ -n "${bin}" ] || die "Archive did not contain a file named ${bin_name}"

mkdir -p "$DEST"
if command -v install >/dev/null 2>&1; then
  install -m 755 "$bin" "${DEST}/${bin_name}"
else
  cp "$bin" "${DEST}/${bin_name}"
  chmod 755 "${DEST}/${bin_name}"
fi

say "Installed ${DEST}/${bin_name}"
case ":$PATH:" in
  *":${DEST}:"*) ;;
  *) say "Add ${DEST} to PATH if this shell cannot see it." ;;
esac

"${DEST}/${bin_name}" --version
