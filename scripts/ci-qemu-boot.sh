#!/usr/bin/env bash
set -eu

if ! systemctl is-active --quiet emergency.target; then
    exit 0
fi

echo 'EXT4_BENCHMARK_BOOT_RECOVERY: emergency mode; retrying the required EFI mount'
journalctl -b -u boot-efi.mount -u systemd-udevd.service --no-pager -n 30 || true
if timeout 180 udevadm settle --timeout=180 &&
    systemctl reset-failed boot-efi.mount &&
    timeout 180 systemctl start boot-efi.mount &&
    mountpoint -q /boot/efi &&
    systemctl default --no-block; then
    echo 'EXT4_BENCHMARK_BOOT_RECOVERED: EFI mounted; resuming normal boot'
else
    echo 'EXT4_BENCHMARK_BOOT_FAILED: required EFI mount recovery failed' >&2
    exit 1
fi
