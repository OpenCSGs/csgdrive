#!/usr/bin/env bash
set -euo pipefail
kernel=${1:?usage: install-kernels.sh KVM_KERNEL OUTPUT_DIRECTORY [PVM_KERNEL]}
output=${2:?output directory is required}
pvm=${3:-$(dirname -- "$kernel")/vmlinux-pvm}
kernel=$(realpath -e -- "$kernel")
if [[ $(basename -- "$kernel") == vmlinux-pvm ]]; then
    echo 'KVM_KERNEL must reference the standard KVM kernel' >&2
    exit 1
fi
if [[ $# -ge 3 && ! -f "$pvm" ]]; then
    echo "PVM kernel does not exist: $pvm" >&2
    exit 1
fi
cp --reflink=auto -- "$kernel" "$output/vmlinux"
cp --reflink=auto -- "$kernel" "$output/vmlinux-bm"
if [[ -f "$pvm" ]]; then
    cp --dereference --reflink=auto -- "$pvm" "$output/vmlinux-pvm"
fi
