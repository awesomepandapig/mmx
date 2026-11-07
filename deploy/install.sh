#!/bin/sh
# Download this script, inspect it, then run: sh install.sh VERSION
set -eu
version=${1:-}
case "$version" in
    ''|*[!0-9A-Za-z.+-]*) echo 'Usage: sh install.sh VERSION (for example 0.1.0-rc.1)' >&2; exit 1 ;;
esac
repository=${MMX_REPOSITORY:-awesomepandapig/sbx}
case "$repository" in
    */*) ;;
    *) echo 'MMX_REPOSITORY must be owner/repository' >&2; exit 1 ;;
esac
case "$(uname -s)" in
    Linux) platform=linux ;;
    Darwin) platform=darwin ;;
    *) echo 'Supported platforms: Linux amd64/arm64 and macOS arm64' >&2; exit 1 ;;
esac
case "$(uname -m)" in
    x86_64|amd64) arch=amd64 ;;
    arm64|aarch64) arch=arm64 ;;
    *) echo 'Unsupported CPU architecture' >&2; exit 1 ;;
esac
if [ "$platform-$arch" = darwin-amd64 ]; then
    echo 'macOS release binaries require Apple silicon (arm64)' >&2
    exit 1
fi
for tool in curl tar install awk; do
    command -v "$tool" >/dev/null 2>&1 || { echo "Required command: $tool" >&2; exit 1; }
done
if command -v sha256sum >/dev/null 2>&1; then
    checksum=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    checksum=shasum
else
    echo 'Install sha256sum or shasum to verify the download' >&2
    exit 1
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
asset="mmx-$version-$platform-$arch.tar.gz"
base="https://github.com/$repository/releases/download/v$version"
curl --fail --location --proto '=https' --tlsv1.2 "$base/$asset" --output "$work/$asset"
curl --fail --location --proto '=https' --tlsv1.2 "$base/SHA256SUMS" --output "$work/SHA256SUMS"
expected=$(awk -v file="$asset" '$2 == file {print $1}' "$work/SHA256SUMS")
case "$expected" in
    ''|*[!0-9a-f]*) echo 'Release has no valid SHA256 checksum for this binary' >&2; exit 1 ;;
esac
[ "${#expected}" -eq 64 ] || { echo 'Invalid SHA256 length' >&2; exit 1; }
if [ "$checksum" = sha256sum ]; then
    actual=$(sha256sum "$work/$asset")
else
    actual=$(shasum -a 256 "$work/$asset")
fi
actual=${actual%% *}
[ "$actual" = "$expected" ] || { echo 'SHA256 verification failed; nothing installed' >&2; exit 1; }
tar -xzf "$work/$asset" -C "$work" mmx
[ -f "$work/mmx" ] && [ ! -L "$work/mmx" ] || { echo 'Invalid release archive' >&2; exit 1; }
mkdir -p "$HOME/.local/bin"
install -m 755 "$work/mmx" "$HOME/.local/bin/mmx"
printf 'Installed %s to %s/.local/bin/mmx\n' "$version" "$HOME"
printf 'Ensure %s/.local/bin is in your PATH, then run mmx --help.\n' "$HOME"
