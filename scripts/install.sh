#!/bin/sh
# Downloads Kelpy from the latest GitHub release, checks it against the release's SHA256SUMS and runs it.
# Kelpy's first run then asks where to install itself. Nothing is installed by this script.
#
#   curl -fsSL https://kelpy.io/install.sh | sh
#
# KELPY_VERSION=v0.1.0 picks a release; arguments after `sh -s --` are passed to Kelpy.

set -eu

repo="https://github.com/Ryadel/Kelpy/releases"

fail() {
    echo "Error: $*" >&2
    exit 1
}

case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    *) fail "Kelpy is available for Linux, macOS and Windows. This system is $(uname -s)." ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) fail "no Kelpy build for the $(uname -m) architecture" ;;
esac
asset="kelpy-$os-$arch"

if [ -n "${KELPY_VERSION:-}" ]; then
    base="$repo/download/$KELPY_VERSION"
else
    base="$repo/latest/download"
fi

if command -v curl >/dev/null 2>&1; then
    download() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    download() { wget -q -O "$2" "$1"; }
else
    fail "curl or wget is required"
fi

dir="$(mktemp -d)"
trap 'rm -rf "$dir"' EXIT

echo "Downloading $asset"
download "$base/$asset" "$dir/kelpy" || fail "cannot download $base/$asset"
download "$base/SHA256SUMS" "$dir/SHA256SUMS" || fail "cannot download $base/SHA256SUMS"

expected="$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1 }' "$dir/SHA256SUMS")"
[ -n "$expected" ] || fail "$asset is not listed in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$dir/kelpy" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$dir/kelpy" | awk '{ print $1 }')"
else
    fail "sha256sum or shasum is required to check the download"
fi
[ "$actual" = "$expected" ] || fail "checksum mismatch for $asset (expected $expected, got $actual)"
chmod +x "$dir/kelpy"

# This script is the shell's standard input, so Kelpy reads its answers from the terminal. Without one (cloud-init,
# CI), it takes every first-run default. Ctrl+C is meant for Kelpy: the shell waits for it to stop, then cleans up.
trap : INT
export KELPY_TEMPORARY_INSTALLER=1
status=0
if [ -t 1 ] && (: </dev/tty) 2>/dev/null; then
    "$dir/kelpy" "$@" </dev/tty || status=$?
else
    "$dir/kelpy" --yes "$@" </dev/null || status=$?
fi
exit "$status"
