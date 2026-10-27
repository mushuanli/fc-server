#!/bin/bash
set -euo pipefail
# Export Debian userspace without mounting a guest-controlled filesystem on host.
output=${1:?usage: build-debian13.sh OUTPUT.ext4 [SIZE_MIB]}
size_mib=${2:-2048}
flavor=${GUEST_FLAVOR:-harness}
[[ $flavor == base || $flavor == harness ]] || { echo 'GUEST_FLAVOR must be base or harness' >&2; exit 1; }
[[ $size_mib =~ ^[0-9]+$ && $size_mib -ge 1024 && $size_mib -le 65536 ]] || {
    echo 'SIZE_MIB must be 1024..65536' >&2; exit 1;
}
[[ ! -e $output ]] || { echo 'Output already exists' >&2; exit 1; }
mkdir -p -- "$(dirname -- "$output")"
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
as_root() {
    if (( EUID == 0 )); then "$@"; else sudo -- "$@"; fi
}
docker_cmd=(docker)
if ! docker info >/dev/null 2>&1; then
    as_root docker info >/dev/null
    docker_cmd=(sudo -- docker)
    (( EUID != 0 )) || docker_cmd=(docker)
fi
mkfs=$(command -v mkfs.ext4 || echo /usr/sbin/mkfs.ext4)
[[ -x $mkfs ]] || { echo 'Install e2fsprogs first' >&2; exit 1; }
work=$(mktemp -d "${TMPDIR:-/var/tmp}/firecracker-image.XXXXXXXX")
container=
cleanup() {
    [[ -z $container ]] || "${docker_cmd[@]}" rm -f "$container" >/dev/null || true
    as_root rm -rf -- "$work"
}
trap cleanup EXIT
# Keep administrator public keys out of the repository and Docker's default context.
python3 - "$script_dir/../guest" "$work/context" <<'PY'
from pathlib import Path
import shutil,sys
shutil.copytree(sys.argv[1], sys.argv[2], ignore=shutil.ignore_patterns('node_modules', '__pycache__'))
PY
python3 "$script_dir/image-info.py" ssh-key > "$work/context/ssh-authorized-keys"
"${docker_cmd[@]}" build --target "$flavor" --tag "mindos-firecracker-debian:13-$flavor" \
    --iidfile "$work/image-id" \
    --build-arg "DEBIAN_MIRROR=${DEBIAN_MIRROR:-http://deb.debian.org/debian}" \
    --build-arg "DEBIAN_SECURITY_MIRROR=${DEBIAN_SECURITY_MIRROR:-http://deb.debian.org/debian-security}" \
    --build-arg "NPM_REGISTRY=${NPM_REGISTRY:-https://registry.npmjs.org}" \
    --build-arg "NODE_IMAGE=${NODE_IMAGE:-node:24.22.1-trixie-slim}" \
    "$work/context"
image_id=$(cat "$work/image-id")
# Test as the guest user without network or credentials, before exporting a clean container.
"${docker_cmd[@]}" run --rm --network none --user 1000:1000 --env HOME=/home/agent \
    --entrypoint /usr/local/libexec/image-check "$image_id" > "$work/probe.json"
container=$("${docker_cmd[@]}" create "$image_id")
mkdir "$work/root"
"${docker_cmd[@]}" export "$container" | as_root tar --numeric-owner -xpf - -C "$work/root"
# Exported container markers make systemd skip VM shutdown filesystem handling.
as_root rm -f -- "$work/root/.dockerenv" "$work/root/run/.containerenv"
as_root ln -sfn /run/systemd/resolve/stub-resolv.conf "$work/root/etc/resolv.conf"
python3 "$script_dir/build-initramfs.py" "$work/root/usr/bin/busybox" \
    "$script_dir/../guest/init-overlay" "$output.initrd"
truncate -s "${size_mib}M" "$work/rootfs.ext4"
as_root "$mkfs" -q -F -m 1 -L debian13 -d "$work/root" "$work/rootfs.ext4"
as_root chown "$(id -u):$(id -g)" "$work/rootfs.ext4"
chmod 0600 "$work/rootfs.ext4"
mv -- "$work/rootfs.ext4" "$output"
python3 "$script_dir/image-info.py" inputs "$script_dir/.." "$size_mib" "$flavor" > "$output.inputs.json"
python3 "$script_dir/image-info.py" report "$output" "$work/probe.json" "$image_id"
echo "Debian 13 image created: $output"
