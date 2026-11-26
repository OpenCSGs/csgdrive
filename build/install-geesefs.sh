#!/bin/sh
set -eu
# Install the upstream client for the host architecture.
dest=${1:-"$HOME/.local/bin"}
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) asset=linux-amd64; digest=8253f0bad8070f504b3ca94a76919d9617252e7ffae04efca9ca54d5cfbaaca8 ;;
    Linux-aarch64|Linux-arm64) asset=linux-arm64; digest=d2a47ea0833c2369e1f7ba92cada1c8caadd0e91144f4219fd0ce6a483e52461 ;;
    Darwin-x86_64) asset=mac-amd64; digest=be628ec055c422f32e66f150d77ba5229af06cda2dab61866e236d794bab3318 ;;
    Darwin-arm64) asset=mac-arm64; digest=ed2cbcbb5d2f7be6e5d9c4fd212772f2af195cfb2a442440191f6b11ca36fb53 ;;
    *) echo "No GeeseFS binary for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac
mkdir -p "$dest"
tmp=$(mktemp "$dest/.geesefs.XXXXXXXX")
trap 'rm -f "$tmp"' EXIT
curl --fail --location --retry 3 --output "$tmp" "https://github.com/yandex-cloud/geesefs/releases/download/v0.43.9/geesefs-$asset"
if command -v sha256sum >/dev/null; then
    actual=$(sha256sum < "$tmp")
else
    actual=$(shasum -a 256 < "$tmp")
fi
[ "${actual%% *}" = "$digest" ] || { echo "GeeseFS SHA-256 mismatch" >&2; exit 1; }
chmod 755 "$tmp"
mv -f "$tmp" "$dest/geesefs"
