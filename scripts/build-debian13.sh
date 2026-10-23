#!/bin/bash
set -euo pipefail
# Export Debian userspace without mounting a guest-controlled filesystem on host.
output=${1:?usage: build-debian13.sh OUTPUT.ext4 [SIZE_MIB]}
size_mib=${2:-4096}
[[ $size_mib =~ ^[0-9]+$ && $size_mib -ge 1024 && $size_mib -le 65536 ]]
[[ ! -e $output ]] || { echo 'Output already exists' >&2; exit 1; }
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
work=$(mktemp -d "${TMPDIR:-/var/tmp}/firecracker-image.XXXXXXXX")
container=
cleanup() {
    [[ -z $container ]] || docker rm -f "$container" >/dev/null
    sudo rm -rf -- "$work"
}
trap cleanup EXIT
docker build --tag mindos-firecracker-debian:13 \
    --build-arg "DEBIAN_MIRROR=${DEBIAN_MIRROR:-http://deb.debian.org/debian}" \
    --build-arg "DEBIAN_SECURITY_MIRROR=${DEBIAN_SECURITY_MIRROR:-http://deb.debian.org/debian-security}" \
    "$script_dir/../guest"
container=$(docker create mindos-firecracker-debian:13)
mkdir "$work/root"
docker export "$container" | sudo tar --numeric-owner -xpf - -C "$work/root"
# Exported container markers make systemd skip VM shutdown filesystem handling.
sudo rm -f -- "$work/root/.dockerenv" "$work/root/run/.containerenv"
sudo ln -sfn /run/systemd/resolve/stub-resolv.conf "$work/root/etc/resolv.conf"
truncate -s "${size_mib}M" "$work/rootfs.ext4"
sudo /usr/sbin/mkfs.ext4 -q -F -L debian13 -d "$work/root" "$work/rootfs.ext4"
sudo chown "$(id -u):$(id -g)" "$work/rootfs.ext4"
chmod 0600 "$work/rootfs.ext4"
mv -- "$work/rootfs.ext4" "$output"
echo "Debian 13 image created: $output"
