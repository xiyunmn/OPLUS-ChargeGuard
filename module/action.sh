#!/system/bin/sh
MODDIR=${0%/*}
"$MODDIR/bin/cg" start
"$MODDIR/bin/cg" status
