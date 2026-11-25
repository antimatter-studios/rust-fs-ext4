#!/usr/bin/env bash
set -euo pipefail

case "${1:-}" in
    x86_64) dependencies=(qemu-system-x86 seabios ipxe-qemu build-essential python3-yaml) ;;
    aarch64) dependencies=(qemu-system-arm qemu-efi-aarch64 ipxe-qemu) ;;
    *) echo 'Expected x86_64 or aarch64 test guest' >&2; exit 2 ;;
esac
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
    qemu-utils cloud-image-utils openssh-client ripgrep rsync time "${dependencies[@]}"

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
read -r version checksum < <(python3 -c '
import json
with open("scripts/qemu-pilot-environment.json") as config:
    oras = json.load(config)["oras"]
print(oras["version"], oras["sha256"]["amd64"])
')
curl -fsSL --retry 3 --connect-timeout 15 --max-time 300 \
    "https://github.com/oras-project/oras/releases/download/v$version/oras_${version}_linux_amd64.tar.gz" -o "$scratch/oras.tar.gz"
printf '%s  %s\n' "$checksum" "$scratch/oras.tar.gz" | sha256sum -c -
tar -xzf "$scratch/oras.tar.gz" -C "$scratch" oras
sudo install -m 0755 "$scratch/oras" /usr/local/bin/oras
