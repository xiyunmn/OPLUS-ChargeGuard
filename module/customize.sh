#!/system/bin/sh
[ "$BOOTMODE" = true ] || abort "请在 Magisk / KernelSU 管理器中安装"
[ "$ARCH" = arm64 ] || abort "仅支持 ARM64"
cg_sdk=$(getprop ro.build.version.sdk)
case "$cg_sdk" in ''|*[!0-9]*) abort "无法识别 Android 版本" ;; esac
[ "$cg_sdk" -ge 35 ] || abort "需要 Android 15 或更新版本"
cg_brand=$(getprop ro.product.brand | tr '[:upper:]' '[:lower:]')
cg_maker=$(getprop ro.product.manufacturer | tr '[:upper:]' '[:lower:]')
case "$cg_brand:$cg_maker" in
  oppo:*|oneplus:*|realme:*|oplus:*|*:oppo|*:oneplus|*:realme|*:oplus) ;;
  *) abort "此版本面向 OPLUS（OPPO / 一加 / realme）设备" ;;
esac
for cg_dir in /data/adb/modules/*; do
  [ -d "$cg_dir" ] || continue
  cg_id=$(sed -n 's/^id=//p' "$cg_dir/module.prop" 2>/dev/null)
  case "$cg_id" in Vehemence|thermal_horae_extreme)
    [ -e "$cg_dir/disable" ] || abort "存在启用的其他温控模块，请先停用并重启" ;;
  esac
done
set_perm "$MODPATH/bin/cg" 0 0 0755
set_perm "$MODPATH/bin/cg-cooling-events" 0 0 0755
set_perm "$MODPATH/bin/charge_guard_pps.ko" 0 0 0644
set_perm "$MODPATH/bin/charge_guard_power.ko" 0 0 0644
for cg_script in service.sh post-fs-data.sh action.sh uninstall.sh; do
  set_perm "$MODPATH/$cg_script" 0 0 0755
done
"$MODPATH/bin/cg" init-config || abort "无法准备配置目录"
cg_name=$(sed -n 's/^name=//p' "$MODPATH/module.prop")
cg_version=$(sed -n 's/^version=//p' "$MODPATH/module.prop")
ui_print "$cg_name  $cg_version"
ui_print "已安装；重启后常驻运行，支持全局及充电独立预设"
