#!/usr/bin/env bash
set -euo pipefail
source_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
output=${1:?usage: build-image.sh NEW_OUTPUT_DIRECTORY KERNEL_PATH [PVM_KERNEL_PATH]}
kernel=${2:?kernel path is required}
if [[ -e "$output" ]]; then echo 'Output directory must not exist' >&2; exit 1; fi
mkdir -p -- "$output"
output=$(realpath -- "$output")
bash "$source_dir/install-kernels.sh" "$kernel" "$output" "${@:3}"
docker build --build-arg "CUBE_BASE_IMAGE=${CUBE_BASE_IMAGE:-ghcr.io/tencentcloud/cubesandbox-base:2026.16}" -t agentdrive-ubuntu:local "$source_dir"
container=$(docker create agentdrive-ubuntu:local)
trap 'docker rm -f "$container" >/dev/null 2>&1 || true' EXIT
docker cp -L "$container:/usr/share/novnc" "$output/novnc"
docker cp "$container:/opt/agentdrive-template.json" "$output/ubuntu.template.json"
docker export "$container" -o "$output/rootfs.tar"
docker run --rm --user 0 -v "$output:/out" --entrypoint bash agentdrive-ubuntu:local -c '
set -euo pipefail
mkdir /out/rootfs
tar -xf /out/rootfs.tar -C /out/rootfs
truncate -s 16G /out/ubuntu.ext4
mkfs.ext4 -F -d /out/rootfs /out/ubuntu.ext4
chown --reference=/out/rootfs.tar /out/ubuntu.ext4
rm -rf /out/rootfs /out/rootfs.tar
'
printf 'Template: %s/ubuntu.ext4\nKernel: %s/vmlinux\n' "$output" "$output"
