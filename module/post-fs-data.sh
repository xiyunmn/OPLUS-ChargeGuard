#!/system/bin/sh
mkdir -p /dev/charge_guard_masks
chmod 0755 /dev/charge_guard_masks
chcon u:object_r:tmpfs:s0 /dev/charge_guard_masks 2>/dev/null
