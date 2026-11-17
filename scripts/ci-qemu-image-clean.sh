#!/usr/bin/env bash
set -euo pipefail
test "${FLTH_GUEST:-}" = 1
test -f /var/lib/ext4-qemu-prepared.json
rm -rf /repo /rust-fs-core /benchmark-target
cloud-init clean --logs --machine-id
: >/etc/machine-id
rm -f /root/.ssh/authorized_keys /root/.ssh/known_hosts /etc/ssh/ssh_host_* /var/lib/systemd/random-seed
rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*.deb
fstrim -v /
echo EXT4_BENCHMARK_IMAGE_CLEANED
systemctl poweroff
