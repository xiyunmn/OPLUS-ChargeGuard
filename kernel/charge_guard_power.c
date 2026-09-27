// SPDX-License-Identifier: GPL-2.0-only
/* Capability-checked additive MIN votes. No OEM client, PDO or protection edits. */
#ifdef CG_POWER_HOST_TEST
#ifdef __KERNEL__
#error Host tests cannot be built into a kernel module
#endif
#include "../tests/power_kernel_shim.h"
#else
#include <linux/module.h>
#include <linux/kprobes.h>
#include <linux/miscdevice.h>
#include <linux/fs.h>
#include <linux/uaccess.h>
#include <linux/mutex.h>
#include <linux/workqueue.h>
#include <linux/ktime.h>
#include <linux/power_supply.h>
#include <linux/platform_device.h>
#include <linux/reboot.h>
#include <linux/of.h>
#include "power_anchors.h"
#endif
#include "power_policy.h"
#define VOTER "CHARGE_GUARD_POWER_VOTER"
#define DRIVER_ID "pjz110-power-205fb7eb-v1"
#define VOTE_TYPE_OFFSET 0x324
struct votable;
struct oplus_mms;
struct mms_subscribe;
union mms_msg_data { int intval; char *strval; };
enum mms_msg_type { MSG_TYPE_TIMER, MSG_TYPE_ITEM };
static int (*cg_vote)(struct votable *, const char *, bool, int, bool);
static struct votable *(*cg_find)(const char *);
static int (*cg_client)(struct votable *, const char *);
static bool (*cg_client_on)(struct votable *, const char *);
static int (*cg_effective)(struct votable *);
static struct oplus_mms *(*cg_topic)(const char *);
static int (*cg_item)(struct oplus_mms *, u32, union mms_msg_data *, bool);
static struct mms_subscribe *(*cg_subscribe)(struct oplus_mms *, void *,
    void (*)(struct mms_subscribe *, enum mms_msg_type, u32, bool), const char *, ...);
static int (*cg_unsubscribe)(struct mms_subscribe *);
static void (*cg_put)(struct oplus_mms *);
static int (*cg_vbus)(void);
static DEFINE_MUTEX(lock);
static struct delayed_work work;
static atomic_t events = ATOMIC_INIT(1);
static bool ready, retiring, enabled, opened, stopping;
static unsigned removing;
static unsigned watts = 50, mask = 1, dirty_votes;
static int protocol, voltage, effective_ma, last_error;
static const char *phase = "off";
static struct cg_filter filter;
static u64 last_valid, last_sample;
static struct module *driver_owner;
static struct power_supply *battery;
static struct oplus_mms *topics[4];
static struct mms_subscribe *subs[4];
static const char *topic_names[] = {"wired", "vooc", "ufcs", "pps"};
static const char *vote_names[] = {NULL, "VOOC_CURR", "UFCS_CURR", "PPS_CURR", "WIRED_ICL"};

static u64 now_ms(void) { return ktime_to_ms(ktime_get_boottime()); }
static void wake(void)
{
    atomic_set(&events, 1);
    if (READ_ONCE(ready) && READ_ONCE(enabled) && !READ_ONCE(stopping))
        mod_delayed_work(system_wq, &work, 0);
}
static void message(struct mms_subscribe *s, enum mms_msg_type type, u32 id, bool sync)
{
    /* Timer publications are not charge/protocol transitions. */
    if (type == MSG_TYPE_ITEM) wake();
}
static int power_event(struct notifier_block *nb, unsigned long action, void *data)
{
    wake();
    return NOTIFY_OK;
}
static struct notifier_block power_nb = { .notifier_call = power_event };
static int item(unsigned t, unsigned id, int *value)
{
    union mms_msg_data data = {0};
    int rc = cg_item(topics[t], id, &data, false);
    if (rc < 0) return rc;
    *value = data.intval;
    return 0;
}
/* Called with lock, including after a failed vote: return values alone cannot
 * establish ownership, because the OEM's global checks may silently reject it. */
static int clear_votes(void)
{
    int p, rc = 0;
    if (removing && dirty_votes) { phase = "restore_pending"; return -EBUSY; }
    for (p = CG_SVOOC; p <= CG_PD; p++) {
        struct votable *v;
        if (!(dirty_votes & BIT(p))) continue;
        v = cg_find(vote_names[p]);
        if (!v) {
            if (retiring) dirty_votes &= ~BIT(p); /* Removed votable owns no live vote. */
            else rc = -ENODEV;
            continue;
        }
        cg_vote(v, VOTER, false, 0, false);
        if (cg_client_on(v, VOTER)) { rc = -EIO; continue; }
        dirty_votes &= ~BIT(p);
    }
    if (!dirty_votes) {
        filter = (struct cg_filter){0};
        effective_ma = 0;
    }
    if (rc) { last_error = rc; phase = "restore_pending"; }
    return rc;
}
static int classify(void)
{
    union power_supply_propval status;
    int wired, type, vooc, sid, ufcs, pps, n, p;
    if (power_supply_get_property(battery, POWER_SUPPLY_PROP_STATUS, &status) < 0)
        return -EIO;
    if (status.intval != POWER_SUPPLY_STATUS_CHARGING) return CG_NONE;
    if (item(0, 1, &wired) || item(0, 3, &type) || item(1, 2, &vooc) ||
        item(1, 3, &sid) || item(2, 1, &ufcs) || item(3, 1, &pps)) return -EIO;
    if (!wired) return CG_NONE;
    /* Never count an adapter's capabilities or online_keep as active charging. */
    n = !!vooc + !!ufcs + !!pps;
    if (n > 1) return -EAGAIN;
    if (vooc) p = (sid & 15) == 3 ? CG_SVOOC : CG_NONE;
    else if (ufcs) p = CG_UFCS;
    else if (pps) p = CG_PPS;
    else p = (type == 6 || type == 7 || type == 8) ? CG_PD : CG_NONE;
    return p;
}
static void maintain(struct work_struct *unused)
{
    u64 now;
    int next, limit_ma, p, sample, changed, rc;
    struct votable *v;
    mutex_lock(&lock);
    if (!ready || retiring || !enabled || stopping) goto out;
    changed = atomic_xchg(&events, 0);
    if (changed) {
        p = classify();
        if (p != protocol) {
            if (clear_votes()) goto retry_restore;
            protocol = p; voltage = 0; last_valid = last_sample = 0;
        }
    }
    if (protocol <= CG_NONE || !(mask & BIT(protocol - 1))) {
        if (clear_votes()) goto retry_restore;
        phase = protocol < 0 ? "data_error" : "waiting_protocol";
        last_error = protocol < 0 ? protocol : 0;
        goto out; /* No timer for inactive or unselected protocols. */
    }
    now = now_ms();
    sample = !last_sample || now - last_sample >= 1000;
    if (sample) {
        last_sample = now;
        voltage = cg_vbus();
        next = cg_limit(protocol, watts, voltage);
        if (next) last_valid = now;
        else {
            filter.candidate = filter.confirmations = 0;
            phase = "data_error"; last_error = -ERANGE;
            if (!last_valid || now - last_valid >= 3000) clear_votes();
            goto again;
        }
    } else {
        next = cg_limit(protocol, watts, voltage);
        if (!next) goto again;
    }
    limit_ma = cg_filter_next(&filter, next, sample);
    if (!limit_ma) { phase = "waiting_voltage"; goto again; }
    if (atomic_read(&events)) goto again;
    v = cg_find(vote_names[protocol]);
    if (!v) { phase = "data_error"; last_error = -ENODEV; goto again; }
    /* Immutable type, verified by vote()'s implementation contract and live
     * field-load instruction. A known name alone cannot prove a MIN election. */
    if (READ_ONCE(*(const int *)((const u8 *)v + VOTE_TYPE_OFFSET)) != 0) {
        phase = "conflict"; last_error = -EINVAL; enabled = false;
        clear_votes(); goto out;
    }
    if (!(dirty_votes & BIT(protocol)) && cg_client_on(v, VOTER)) {
        phase = "conflict"; last_error = -EBUSY; enabled = false; goto out;
    }
    if (limit_ma != filter.applied || !cg_client_on(v, VOTER) || cg_client(v, VOTER) != limit_ma) {
        dirty_votes |= BIT(protocol); /* May mutate before returning an error. */
        rc = cg_vote(v, VOTER, true, limit_ma, false);
        if (rc < 0 || !cg_client_on(v, VOTER) || cg_client(v, VOTER) != limit_ma) {
            last_error = rc < 0 ? rc : -EIO; phase = "vote_rejected";
            clear_votes(); goto again;
        }
        filter.applied = limit_ma;
    }
    effective_ma = cg_effective(v);
    if (effective_ma < 0 || effective_ma > limit_ma) {
        /* Override/force mode from another component must not look applied. */
        phase = "conflict"; last_error = -EBUSY; clear_votes(); goto again;
    }
    phase = "applied"; last_error = 0;
again:
    if (enabled && !stopping)
        mod_delayed_work(system_wq, &work, atomic_read(&events) ? 0 : msecs_to_jiffies(1000));
    goto out;
retry_restore:
    atomic_set(&events, 1);
    /* Removal is idempotent and never raises another client's limit. */
    mod_delayed_work(system_wq, &work, msecs_to_jiffies(1000));
out:
    mutex_unlock(&lock);
}
static void release_topics(void)
{
    int i;
    for (i = 0; i < 4; i++) {
        if (subs[i]) { cg_unsubscribe(subs[i]); subs[i] = NULL; }
        if (topics[i]) { cg_put(topics[i]); topics[i] = NULL; }
    }
}
static int bus_event(struct notifier_block *nb, unsigned long action, void *data)
{
    if (action == BUS_NOTIFY_UNBOUND_DRIVER) {
        mutex_lock(&lock);
        if (removing) removing--;
        if (!removing && !clear_votes()) phase = "retired";
        mutex_unlock(&lock);
        return NOTIFY_OK;
    }
    if (action != BUS_NOTIFY_UNBIND_DRIVER) return NOTIFY_DONE;
    /* Blocking platform-bus notification precedes remove/devres. Retire on any
     * platform unbind, avoiding private chip pointers or sleeping kprobes. */
    mutex_lock(&lock);
    retiring = true; enabled = false; ready = false;
    clear_votes(); release_topics();
    removing++;
    if (!dirty_votes) phase = "retired";
    mutex_unlock(&lock);
    cancel_delayed_work_sync(&work);
    return NOTIFY_OK;
}
static struct notifier_block bus_nb = { .notifier_call = bus_event };
static int reboot_event(struct notifier_block *nb, unsigned long action, void *data)
{
    /* Runs before device_shutdown, while every OEM consumer is still alive. */
    mutex_lock(&lock);
    stopping = true; ready = enabled = false;
    clear_votes(); release_topics();
    if (!dirty_votes) phase = "retired";
    mutex_unlock(&lock);
    cancel_delayed_work_sync(&work);
    return NOTIFY_OK;
}
static struct notifier_block reboot_nb = { .notifier_call = reboot_event };

static int control_open(struct inode *inode, struct file *file)
{
    int rc = 0;
    mutex_lock(&lock);
    if (removing) rc = -EBUSY;
    else if ((!ready || retiring) && !dirty_votes) rc = -ENODEV;
    else if (opened) rc = -EBUSY;
    else opened = true;
    mutex_unlock(&lock);
    return rc;
}
static ssize_t control_write(struct file *file, const char __user *buffer, size_t size, loff_t *pos)
{
    char text[64], extra;
    unsigned w, m;
    int rc = 0;
    bool off;
    if (!size || size >= sizeof(text)) return -EINVAL;
    if (copy_from_user(text, buffer, size)) return -EFAULT;
    text[size] = 0;
    off = !strcmp(text, "0\n") || !strcmp(text, "0");
    if (!off && (sscanf(text, "1 %u %u %c", &w, &m, &extra) != 2 ||
        w < 20 || w > 100 || !m || m > 15)) return -EINVAL;
    mutex_lock(&lock);
    if (off) {
        enabled = false; rc = clear_votes();
        if (!rc) phase = "off";
    } else if (!ready || retiring) rc = -ENODEV;
    else if (dirty_votes && !enabled) rc = -EBUSY;
    else {
        watts = w; mask = m; enabled = true;
        filter.candidate = filter.confirmations = 0;
        last_sample = 0; phase = "waiting_voltage";
        wake();
    }
    mutex_unlock(&lock);
    if (off) cancel_delayed_work_sync(&work);
    return rc ? rc : size;
}
static int control_release(struct inode *inode, struct file *file)
{
    mutex_lock(&lock);
    enabled = false;
    if (!clear_votes()) phase = retiring ? "retired" : "off";
    mutex_unlock(&lock);
    cancel_delayed_work_sync(&work);
    mutex_lock(&lock); opened = false; mutex_unlock(&lock);
    return 0;
}
static const struct file_operations fops = {
    .owner = THIS_MODULE, .open = control_open, .write = control_write,
    .release = control_release, .llseek = no_llseek,
};
static struct miscdevice control = { .minor = MISC_DYNAMIC_MINOR,
    .name = "charge_guard_power", .fops = &fops, .mode = 0600 };
static int get_status(char *buffer, const struct kernel_param *param)
{
    int n;
    mutex_lock(&lock);
    n = snprintf(buffer, PAGE_SIZE,
        "api=1 driver=" DRIVER_ID " ready=%u enabled=%u session=%u watts=%u mask=%u protocol=%d voltage_mv=%d cap_ma=%d effective_ma=%d pending=%u state=%s error=%d\n",
        ready, enabled, opened, watts, mask, protocol, voltage, filter.applied,
        effective_ma, !!dirty_votes, phase, last_error);
    mutex_unlock(&lock);
    return n;
}
static const struct kernel_param_ops status_ops = { .get = get_status };
module_param_cb(status, &status_ops, NULL, 0400);

static int resolve(void)
{
    void **dest[] = {(void **)&cg_vote,(void **)&cg_find,(void **)&cg_client,
        (void **)&cg_client_on,(void **)&cg_effective,(void **)&cg_topic,
        (void **)&cg_item,(void **)&cg_subscribe,(void **)&cg_unsubscribe,
        (void **)&cg_put,(void **)&cg_vbus};
    int i, rc = 0;
    for (i = 0; i < ARRAY_SIZE(cg_anchors); i++) {
        struct kprobe resolver = { .symbol_name = cg_anchors[i].symbol };
        u32 *address;
        rc = register_kprobe(&resolver);
        if (rc) break;
        address = (u32 *)resolver.addr;
        /* Restore the entry instruction before examining it. pin_driver() holds
         * the OEM module alive throughout resolution and subsequent calls. */
        unregister_kprobe(&resolver);
        if (READ_ONCE(address[-1]) != cg_anchors[i].kcfi ||
            READ_ONCE(address[0]) != cg_anchors[i].words[0] ||
            READ_ONCE(address[1]) != cg_anchors[i].words[1] ||
            READ_ONCE(address[2]) != cg_anchors[i].words[2]) { rc = -ENOEXEC; break; }
        *dest[i] = address;
    }
    if (!rc && READ_ONCE(*(const u32 *)((const u8 *)cg_vote + 0x130)) != 0xb9432688)
        rc = -ENOEXEC; /* ldr w8, [x20, #0x324]: votable->type */
    return rc;
}
static int bind_topics(void)
{
    struct device *parents[4] = {0};
    int i, locked = 0, rc = -ENODEV;
    for (i = 0; i < 4; i++) {
        topics[i] = cg_topic(topic_names[i]);
        if (!topics[i]) goto out;
        /* Embedded struct device offset verified against this firmware. A topic
         * reference keeps the device alive; a trylock rejects ongoing unbind. */
        parents[i] = ((struct device *)((u8 *)topics[i] + 488))->parent;
        if (!parents[i] || !device_trylock(parents[i])) goto out;
        locked++;
        if (!parents[i]->driver) goto out;
        if (!driver_owner) {
            driver_owner = parents[i]->driver->owner;
            if (!driver_owner || !try_module_get(driver_owner)) { driver_owner = NULL; goto out; }
        } else if (parents[i]->driver->owner != driver_owner) goto out;
    }
    for (i = 0; i < 4; i++) {
        subs[i] = cg_subscribe(topics[i], &control, message, "charge_guard_power");
        if (IS_ERR(subs[i])) { rc = PTR_ERR(subs[i]); subs[i] = NULL; goto out; }
    }
    rc = 0;
out:
    if (rc) release_topics();
    while (locked) device_unlock(parents[--locked]);
    return rc;
}
static bool matching_tree(void)
{
    struct device_node *node = of_find_node_by_path("/soc/oplus,vooc");
    u32 width, table, maximum;
    bool ok = node && !of_property_read_u32(node, "oplus,vooc_data_width", &width)
        && !of_property_read_u32(node, "oplus,vooc_curr_table_type", &table)
        && !of_property_read_u32(node, "oplus,vooc_curr_max", &maximum)
        && width == 7 && table == 2 && maximum == 19;
    of_node_put(node);
    return ok;
}
/* Pin code before resolving or calling any private OEM interface. Parent device
 * trylock also rejects an unbind that began before our bus notifier registered. */
static int pin_driver(void)
{
    struct device_node *node = of_find_node_by_path("/soc/oplus,vooc");
    struct device *dev;
    int rc = -ENODEV;
    if (!node) return rc;
    dev = bus_find_device_by_of_node(&platform_bus_type, node);
    of_node_put(node);
    if (!dev) return rc;
    if (device_trylock(dev)) {
        if (dev->driver && dev->driver->owner &&
            !strcmp(dev->driver->owner->name, "oplus_chg_v2") &&
            try_module_get(dev->driver->owner)) {
            driver_owner = dev->driver->owner; rc = 0;
        }
        device_unlock(dev);
    }
    put_device(dev);
    return rc;
}
static int __init cg_power_init(void)
{
    int rc;
    INIT_DELAYED_WORK(&work, maintain);
    if (!matching_tree()) return -ENODEV;
    rc = bus_register_notifier(&platform_bus_type, &bus_nb); if (rc) return rc;
    mutex_lock(&lock);
    rc = retiring ? -ENODEV : pin_driver();
    if (!rc) rc = resolve();
    if (!rc) rc = bind_topics();
    if (!rc) {
        battery = power_supply_get_by_name("battery");
        if (!battery) rc = -ENODEV;
    }
    mutex_unlock(&lock);
    if (rc) goto fail;
    rc = power_supply_reg_notifier(&power_nb); if (rc) goto fail;
    rc = register_reboot_notifier(&reboot_nb);
    if (rc) { power_supply_unreg_notifier(&power_nb); goto fail; }
    rc = misc_register(&control);
    if (rc) { unregister_reboot_notifier(&reboot_nb); power_supply_unreg_notifier(&power_nb); goto fail; }
    mutex_lock(&lock);
    rc = retiring ? -ENODEV : 0;
    if (!rc) ready = true;
    mutex_unlock(&lock);
    if (rc) { misc_deregister(&control); unregister_reboot_notifier(&reboot_nb); power_supply_unreg_notifier(&power_nb); goto fail; }
    return 0;
fail:
    bus_unregister_notifier(&platform_bus_type, &bus_nb);
    release_topics();
    if (battery) power_supply_put(battery);
    if (driver_owner) module_put(driver_owner);
    return rc;
}
static void __exit cg_power_exit(void)
{
    WRITE_ONCE(stopping, true);
    misc_deregister(&control);
    unregister_reboot_notifier(&reboot_nb);
    power_supply_unreg_notifier(&power_nb);
    bus_unregister_notifier(&platform_bus_type, &bus_nb);
    cancel_delayed_work_sync(&work);
    mutex_lock(&lock);
    enabled = ready = false;
    clear_votes(); release_topics();
    mutex_unlock(&lock);
    power_supply_put(battery);
    module_put(driver_owner);
}
module_init(cg_power_init);
module_exit(cg_power_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Capability-checked additive charge input power ceiling");
