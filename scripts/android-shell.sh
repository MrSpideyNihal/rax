#!/bin/sh
# Boots the Android test image and runs commands from a file in its shell.
# Usage: sh scripts/android-shell.sh COMMANDS_FILE [LOG] [EXTRA_KERNEL_ARGS]
# Run from the repository root in Git Bash, with guest files in guest/.
set -u

commands=$1
log=${2:-guest/android-shell.log}
extra=${3:-}
wait_secs=${WAIT_SECS:-40}
step_secs=${STEP_SECS:-2}
ram_mib=${RAM_MIB:-1536}

# GPU=folder boots on the virtio GPU, which needs kernel mode setting.
if [ -n "${GPU:-}" ]; then
    graphics=""
else
    graphics="nomodeset HWACCEL=0"
fi

cmdline="console=ttyS0 earlyprintk=serial,ttyS0,115200 nolapic noapic acpi=off \
nosmp reboot=k panic=-1 i8042.noaux tsc=reliable tsc_early_khz=3194043 \
androidboot.hardware=android_x86_64 androidboot.console=ttyS0 \
androidboot.selinux=permissive $graphics SRC= DATA= DEBUG= $extra"

lines=$(wc -l < "$commands")
timeout=$((wait_secs + (lines + 2) * step_secs + 10 + ${EXTRA_SECS:-0}))

(
    sleep "$wait_secs"
    while IFS= read -r line; do
        printf '%s\n' "$line"
        sleep "$step_secs"
    done < "$commands"
    echo 'reboot -p'
    sleep 2
) | ./target/release/rax.exe vm run --shell --kernel guest/kernel \
    --initrd guest/initrd.img --disk guest/android-x86_64-9.0-r2.iso \
    --ram-mib "$ram_mib" --timeout-secs "$timeout" --cmdline "$cmdline" \
    ${NET:+--net} ${GPU:+--gpu "$GPU"} --framebuffer "${SCREEN:-1280x720}" \
    --screenshot "$log.bmp" \
    --report "$log.json" > "$log" 2>&1
echo "exit $?"
