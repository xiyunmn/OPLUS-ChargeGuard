#!/system/bin/sh
MODDIR=${0%/*}
cg_name=$(sed -n 's/^name=//p' "$MODDIR/module.prop")
"$MODDIR/bin/cg" recover || {
  echo "${cg_name}：存在未完成恢复；记录保留于 /data/adb/charge_guard，请重启后核对。" >&2
  exit 1
}
# Legacy persistent values without a verified original value are never guessed or rewritten.
