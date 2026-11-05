// SPDX-License-Identifier: GPL-2.0-only
/* OEM PPS-status assistance and bounded CPA re-admission. No direct PDO/vote writes. */
#include <linux/module.h>
#include <linux/kprobes.h>
#include <linux/moduleparam.h>
#include <linux/spinlock.h>
#include <linux/string.h>

#define STATUS_OFFSET 3108
#define MONITOR_WORK_OFFSET 400
#define SWITCH_WORK_OFFSET 264
#define DRIVER_ID "pjz110-f04-205fb7eb-v1"

static DEFINE_RAW_SPINLOCK(state_lock);
static bool ready, enabled, retiring, original;
static u8 *bound_chip;
static unsigned long observations;
static struct kprobe probes[4];

#include "pps_diagnostics.h"
#include "pps_recovery.h"

/* Only state_lock holders access the saved pointer. remove/shutdown invalidate it
 * before the OEM driver can free it, and prevent later workers from rebinding. */
static void restore_locked(void)
{
    if (bound_chip && READ_ONCE(bound_chip[STATUS_OFFSET]) == 1)
        WRITE_ONCE(bound_chip[STATUS_OFFSET], original);
    bound_chip = NULL;
}

static int assist_pre(struct kprobe *probe, struct pt_regs *regs)
{
    unsigned long flags;
    u8 *chip;
    raw_spin_lock_irqsave(&state_lock, flags);
    if (!ready || !enabled || retiring)
        goto out;
    chip = (u8 *)regs->regs[0] -
        (probe == &probes[0] ? MONITOR_WORK_OFFSET : SWITCH_WORK_OFFSET);
    /* The field is a bool in the fingerprinted OEM layout. Refuse an unexpected
     * instance instead of restoring or writing through an unrecognised pointer. */
    if ((bound_chip && bound_chip != chip) || READ_ONCE(chip[STATUS_OFFSET]) > 1) {
        retiring = true;
        enabled = false;
        restore_locked();
        goto out;
    }
    if (!bound_chip) {
        original = READ_ONCE(chip[STATUS_OFFSET]);
        bound_chip = chip;
        diag_bound(chip, true);
    } else {
        diag_bound(chip, false);
    }
    WRITE_ONCE(chip[STATUS_OFFSET], 1);
    observations++;
out:
    raw_spin_unlock_irqrestore(&state_lock, flags);
    return 0;
}
NOKPROBE_SYMBOL(assist_pre);

static int retire_pre(struct kprobe *probe, struct pt_regs *regs)
{
    unsigned long flags;
    (void)probe;
    (void)regs;
    raw_spin_lock_irqsave(&state_lock, flags);
    if (!retiring) diag_emit(DIAG_RETIRING, 0, 0, 0, NULL);
    retiring = true;
    enabled = false;
    restore_locked();
    raw_spin_unlock_irqrestore(&state_lock, flags);
    return 0;
}
NOKPROBE_SYMBOL(retire_pre);

static int set_enabled(const char *value, const struct kernel_param *param)
{
    unsigned long flags;
    bool next;
    bool changed = false;
    int ret = 0;
    (void)param;
    if (!value || (value[0] != '0' && value[0] != '1') ||
        (value[1] != '\0' && !(value[1] == '\n' && value[2] == '\0')))
        return -EINVAL;
    next = value[0] == '1';
    mutex_lock(&rec_lock);
    raw_spin_lock_irqsave(&state_lock, flags);
    if (next && (!ready || retiring)) {
        ret = -ENODEV;
    } else {
        if (enabled != next) {
            changed = true;
            diag.session_used = false;
            diag.ever_charging = false;
            diag.exit_ms = diag.last_reset_ms = diag.last_charging_ms = 0;
            diag_emit(DIAG_ENABLED, 0, next, 0, NULL);
        }
        enabled = next;
        if (!next)
            restore_locked();
    }
    raw_spin_unlock_irqrestore(&state_lock, flags);
    if (changed)
        rec_reset_session(next);
    mutex_unlock(&rec_lock);
    return ret;
}

static int get_enabled(char *buf, const struct kernel_param *param)
{
    (void)param;
    return snprintf(buf, PAGE_SIZE, "%u\n", READ_ONCE(enabled));
}

static int get_status(char *buf, const struct kernel_param *param)
{
    unsigned long flags;
    int count;
    (void)param;
    raw_spin_lock_irqsave(&state_lock, flags);
    count = snprintf(buf, PAGE_SIZE,
        "api=1 driver=" DRIVER_ID " ready=%u enabled=%u applied=%u retiring=%u observations=%lu\n",
        ready, enabled, bound_chip != NULL, retiring, observations);
    raw_spin_unlock_irqrestore(&state_lock, flags);
    return count;
}

static const struct kernel_param_ops enable_ops = { .set = set_enabled, .get = get_enabled };
static const struct kernel_param_ops status_ops = { .get = get_status };
module_param_cb(enabled, &enable_ops, NULL, 0600);
module_param_cb(status, &status_ops, NULL, 0400);

/* Resolve every function independently: unrelated link-order/size changes must
 * not turn the original relative addresses into pointers into another function.
 * Userspace checks only the relevant interface contracts and loaded build ID. */
static bool matching_layout(void)
{
    const u8 *base = (const u8 *)probes[0].addr;
    static const struct { const char *symbol; unsigned int offset; u32 word; } anchors[] = {
        { "oplus_chg_v2:oplus_third_pps_target_voltage_check", 0x1cc, 0x39709269 },
        { "oplus_chg_v2:oplus_third_pps_target_voltage_check", 0x214, 0x52800c88 },
        { "oplus_chg_v2:oplus_third_pps_target_voltage_check", 0x320, 0x7100113f },
        { "oplus_chg_v2:oplus_third_pps_target_voltage_check", 0x350, 0x392fb268 },
        { "oplus_chg_v2:oplus_pps_gauge_update_work", 0x5c, 0x7100211f },
        { "oplus_chg_v2:oplus_pps_gauge_update_work", 0xb4, 0x3109651f },
        { "oplus_chg_v2:oplus_pps_gauge_update_work", 0xc0, 0x71000d1f },
        { "oplus_chg_v2:oplus_pps_charge_allow_check", 0x78, 0x51002929 },
        { "oplus_chg_v2:oplus_pps_charge_allow_check", 0x88, 0x51002929 },
    };
    unsigned int i;
    if (READ_ONCE(*(const u32 *)(base + 0x34)) != 0xd1064014 ||
        READ_ONCE(*(const u32 *)(base + 0x29a8)) != 0x396a5268 ||
        READ_ONCE(*(const u32 *)(base + 0x31c4)) != 0x396a5268)
        return false;
    for (i = 0; i < ARRAY_SIZE(anchors); i++) {
        struct kprobe check = { .symbol_name = anchors[i].symbol };
        bool ok;
        if (register_kprobe(&check)) return false;
        ok = READ_ONCE(*(const u32 *)((const u8 *)check.addr + anchors[i].offset)) == anchors[i].word;
        unregister_kprobe(&check);
        if (!ok)
            return false;
    }
    return true;
}

static int __init cg_pps_init(void)
{
    static const char * const symbols[] = {
        "oplus_chg_v2:oplus_pps_monitor_work",
        "oplus_chg_v2:oplus_pps_switch_check_work",
        "oplus_chg_v2:oplus_pps_remove",
        "oplus_chg_v2:oplus_pps_shutdown",
    };
    int i, ret;
    /* Optional recovery pins the OEM owner before private symbol resolution. */
    rec_init();
    for (i = 0; i < ARRAY_SIZE(probes); i++) {
        probes[i].symbol_name = symbols[i];
        probes[i].pre_handler = i < 2 ? assist_pre : retire_pre;
        ret = register_kprobe(&probes[i]);
        if (ret)
            goto undo;
    }
    if (!matching_layout()) {
        ret = -ENOEXEC;
        goto undo;
    }
    diag_init();
    WRITE_ONCE(ready, true);
    return 0;
undo:
    while (i-- > 0)
        unregister_kprobe(&probes[i]);
    rec_cleanup();
    return ret;
}

static void __exit cg_pps_exit(void)
{
    unsigned long flags;
    int i;
    mutex_lock(&rec_lock);
    raw_spin_lock_irqsave(&state_lock, flags);
    ready = false;
    enabled = false;
    restore_locked();
    raw_spin_unlock_irqrestore(&state_lock, flags);
    rec_reset_session(false);
    mutex_unlock(&rec_lock);
    diag_cleanup();
    for (i = ARRAY_SIZE(probes) - 1; i >= 0; i--)
        unregister_kprobe(&probes[i]);
    rec_cleanup();
}

module_init(cg_pps_init);
module_exit(cg_pps_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("ChargeGuard OEM PPS assistance and bounded re-admission; preserves OEM protection");
MODULE_VERSION("2");
