#!/bin/sh
# Times Android boots. The guest measures itself, from kernel start until
# sys.boot_completed, using /proc/uptime.
# Usage: sh scripts/android-boot-time.sh SESSION_DIR [PASSES] [RAM_MIB]
# Run from the repository root in Git Bash.
set -u

session=$1
passes=${2:-3}
ram_mib=${3:-2048}
mkdir -p "$session"

cmdline="console=ttyS0 nolapic noapic acpi=off nosmp reboot=k panic=-1 \
i8042.noaux tsc=reliable tsc_early_khz=3194043 \
androidboot.hardware=android_x86_64 androidboot.console=ttyS0 \
androidboot.selinux=permissive nomodeset HWACCEL=0 SRC= DATA= DEBUG="

probe='while [ "$(getprop sys.boot_completed)" != "1" ]; do sleep 1; done; echo BOOT_DONE_$(cut -d. -f1 /proc/uptime); reboot -p'

pass=1
while [ "$pass" -le "$passes" ]; do
    log=guest/boot-time-$pass.log
    # The wait matters: Linux clears the serial FIFO when it opens the port,
    # so anything sent earlier is lost.
    (sleep 8; printf '%s\n' "$probe"; sleep 1) | ./target/release/rax.exe vm run \
        --shell --kernel guest/kernel --initrd guest/initrd.img \
        --disk guest/android-x86_64-9.0-r2.iso --ram-mib "$ram_mib" \
        --framebuffer 1280x720 --timeout-secs 180 --cmdline "$cmdline" \
        --report "$session/boot-$pass.json" > "$log" 2>&1
    seconds=$(grep -a -o -E '^BOOT_DONE_[0-9]+' "$log" | head -n 1 | sed 's/BOOT_DONE_//')
    echo "pass $pass: boot_completed at ${seconds:-never} s"
    printf '{\n  "schema_version": 1,\n  "benchmark": "android-rax-boot",\n  "pass": %s,\n  "ram_mib": %s,\n  "boot_completed_secs": %s\n}\n' \
        "$pass" "$ram_mib" "${seconds:-null}" > "$session/pass-$pass.json"
    pass=$((pass + 1))
done
