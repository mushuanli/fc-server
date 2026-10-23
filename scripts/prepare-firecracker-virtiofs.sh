#!/bin/bash
set -euo pipefail
# Reconstruct the tested fork without changing an existing checkout.
output=${1:?usage: prepare-firecracker-virtiofs.sh NEW_SOURCE_DIRECTORY}
[[ ! -e $output ]] || { echo 'Output directory already exists' >&2; exit 1; }
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
base=edb60617c31ebd610c530f67706ec5c79d4c2725
git init --quiet "$output"
git -C "$output" remote add origin https://github.com/firecracker-microvm/firecracker.git
git -C "$output" fetch --depth 1 origin "$base"
git -C "$output" checkout --detach FETCH_HEAD
git -C "$output" apply --check "$script_dir/../patches/firecracker-virtiofs.patch"
git -C "$output" apply "$script_dir/../patches/firecracker-virtiofs.patch"
echo 'Prepared the pinned Firecracker virtio-fs source. Build the musl release as documented.'
