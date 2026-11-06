#!/bin/sh
# Measures guest disk throughput. Run from the repository root in Git Bash.
# Creates a sparse 256 MiB image under guest/, and deletes it afterwards.
set -eu

image=guest/speed.img
truncate -s 256M "$image"
trap 'rm -f "$image"' EXIT

(
    sleep 2
    echo 'time dd if=/dev/zero of=/dev/vda bs=1M count=128 conv=fsync'
    sleep 8
    echo 'echo 3 > /proc/sys/vm/drop_caches'
    sleep 1
    echo 'time dd if=/dev/vda of=/dev/null bs=1M count=128'
    sleep 6
    echo 'poweroff -f'
) | ./target/release/rax.exe vm run --shell --kernel guest/vmlinuz-virt \
    --initrd guest/initramfs-virt --disk "$image" --ram-mib 512 \
    --timeout-secs 30 --report "${1:-guest/speed.json}"
