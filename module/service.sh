#!/system/bin/sh
MODDIR=${0%/*}
[ -e "$MODDIR/disable" ] && exit 0
[ -e "$MODDIR/remove" ] && exit 0
cg_wait=0
while [ "$(getprop sys.boot_completed)" != 1 ]; do
  sleep 1
  cg_wait=$((cg_wait + 1))
  [ "$cg_wait" -ge 120 ] && break
done
sleep 3
# Rust guardian owns startup retries and the 120-second crash restart interval.
"$MODDIR/bin/cg" daemon </dev/null >> /data/adb/charge_guard/startup.log 2>&1 &
