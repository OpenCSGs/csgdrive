#!/usr/bin/env bash
set -euo pipefail
source_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
output=${1:?usage: build-assistant-image.sh NEW_OUTPUT_DIRECTORY KERNEL_PATH [PVM_KERNEL_PATH]}
kernel=${2:?kernel path is required}
[[ ! -e "$output" ]] || { echo 'Output directory must not exist' >&2; exit 1; }
archive="$source_dir/.cache/csgclaw-release.tar.gz"
mkdir -p "$source_dir/.cache"
if [[ ! -f "$archive" ]]; then
    curl --fail --location --retry 3 \
        https://github.com/OpenCSGs/csgclaw/releases/download/v0.10.0/csgclaw_v0.10.0_linux_amd64.tar.gz \
        --output "$archive.download"
    mv "$archive.download" "$archive"
fi
printf '%s  %s\n' 80083df3224a5d9727c43ce86e580a998a5009f37f1d8446aee487eaf6ae9b97 "$archive" | sha256sum -c -
docker build -f "$source_dir/assistant/Dockerfile" -t agentdrive-ubuntu-csgclaw:local "$source_dir"
mkdir -p -- "$output"
output=$(realpath -- "$output")
bash "$source_dir/install-kernels.sh" "$kernel" "$output" "${@:3}"
container=$(docker create agentdrive-ubuntu-csgclaw:local)
trap 'docker rm -f "$container" >/dev/null 2>&1 || true' EXIT
docker cp -L "$container:/usr/share/novnc" "$output/novnc"
docker cp "$container:/opt/agentdrive-template.json" "$output/ubuntu.template.json"
docker export "$container" -o "$output/rootfs.tar"
docker run --rm --user 0 -v "$output:/out" --entrypoint bash agentdrive-ubuntu-csgclaw:local -c '
set -euo pipefail
mkdir /out/rootfs
tar -xf /out/rootfs.tar -C /out/rootfs
truncate -s 16G /out/ubuntu.ext4
mkfs.ext4 -F -d /out/rootfs /out/ubuntu.ext4
chown --reference=/out/rootfs.tar /out/ubuntu.ext4
rm -rf /out/rootfs /out/rootfs.tar
'
printf 'Template: %s/ubuntu.ext4\n' "$output"
