#!/usr/bin/env bash
# Downloads the PDFium build pinned in scripts/pdfium.lock into vendor/pdfium.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
lock="$root/scripts/pdfium.lock"
version="$(grep '^version=' "$lock" | cut -d= -f2)"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) platform=linux-x64 ;;
  *) echo "error: unsupported platform $(uname -s)-$(uname -m); on Windows use scripts/fetch-pdfium.ps1" >&2; exit 1 ;;
esac

expected="$(grep "^$platform=" "$lock" | cut -d= -f2 || true)"
if [ -z "$expected" ]; then
  echo "error: no checksum pinned for $platform in scripts/pdfium.lock" >&2
  exit 1
fi

dest="$root/vendor/pdfium"
if [ -f "$dest/VERSION" ] && grep -qx "BUILD=$version" "$dest/VERSION"; then
  echo "PDFium $version already present in $dest"
  exit 0
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
url="https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/$version/pdfium-$platform.tgz"
echo "Downloading $url"
curl -fsSL "$url" -o "$tmp/pdfium.tgz"

actual="$(sha256sum "$tmp/pdfium.tgz" | cut -d' ' -f1)"
if [ "$actual" != "$expected" ]; then
  echo "error: checksum mismatch for $platform: expected $expected, got $actual" >&2
  exit 1
fi

rm -rf "$dest"
mkdir -p "$dest"
tar -xzf "$tmp/pdfium.tgz" -C "$dest"
echo "PDFium $version ready in $dest"
