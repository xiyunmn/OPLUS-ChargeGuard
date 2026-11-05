// SPDX-License-Identifier: GPL-2.0-only
/* Preserve the real display state; change only the shared charging LED output. */
#include <linux/module.h>
#include <linux/kprobes.h>
#include <linux/miscdevice.h>
#include <linux/fs.h>
#include <linux/uaccess.h>
#include <linux/mutex.h>
#include <linux/spinlock.h>
#include <linux/workqueue.h>
#include <linux/jiffies.h>
#include <linux/power_supply.h>
#include <linux/platform_device.h>
#include <linux/reboot.h>
#include <linux/of.h>
#include <linux/slab.h>
#include "screen_anchors.h"

#define DRIVER_ID "charge-guard-screen-v1"
#define LED_ITEM 16
#define TOPIC_DEVICE_OFFSET 488
#define ITEM_SIZE 128
#define RESTORE_RETRIES 3
struct oplus_mms;
struct mms_subscribe;
struct mms_msg;
union mms_msg_data { int intval; char *strval; };
enum mms_msg_type { MSG_TYPE_TIMER, MSG_TYPE_ITEM };
enum mms_msg_prio { MSG_PRIO_HIGH, MSG_PRIO_MEDIUM, MSG_PRIO_LOW };

static struct oplus_mms *(*cg_topic)(const char *);
static int (*cg_item)(struct oplus_mms *, u32, union mms_msg_data *, bool);
static struct mms_subscribe *(*cg_subscribe)(struct oplus_mms *, void *,
    void (*)(struct mms_subscribe *, enum mms_msg_type, u32, bool), const char *, ...);
static int (*cg_unsubscribe)(struct mms_subscribe *);
static void (*cg_put)(struct oplus_mms *);
static struct mms_msg *(*cg_alloc)(enum mms_msg_type, enum mms_msg_prio, u32);
static int (*cg_publish)(struct oplus_mms *, struct mms_msg *);
static void *led_getter;

static DEFINE_MUTEX(lock);
static DEFINE_RAW_SPINLOCK(observation_lock);
static struct delayed_work work;
static struct delayed_work restore_work;
static unsigned int restore_attempts;
static unsigned long restore_due;
static bool restore_scheduled;
static bool ready, enabled, opened, active, retiring, stopping;
static bool needs_restore, self_held, hook_registered, common_unbinding;
static int original_led = -1, effective_led = -1, last_error;
static u64 observations, publications;
static const char *phase = "off";
static struct module *driver_owner;
static struct power_supply *battery;
static struct oplus_mms *topics[2]; /* common, wired */
static struct mms_subscribe *common_sub, *wired_sub;
static struct device *parents[2];
static u8 *led_item;
static struct kprobe led_probe;
static const char *topic_names[] = { "common", "wired" };

static int led_pre(struct kprobe *probe, struct pt_regs *regs)
{
    unsigned long flags;
    unsigned real = (u32)regs->regs[8];
    raw_spin_lock_irqsave(&observation_lock, flags);
    original_led = real <= 1 ? (int)real : -1;
    if (real <= 1 && active)
        regs->regs[8] = 0;
    effective_led = real <= 1 ? (int)(u32)regs->regs[8] : -1;
    observations++;
    raw_spin_unlock_irqrestore(&observation_lock, flags);
    return 0;
}
NOKPROBE_SYMBOL(led_pre);

static void set_active(bool value)
{
    unsigned long flags;
    raw_spin_lock_irqsave(&observation_lock, flags);
    active = value;
    raw_spin_unlock_irqrestore(&observation_lock, flags);
}
static u64 observation_count(void)
{
    unsigned long flags;
    u64 value;
    raw_spin_lock_irqsave(&observation_lock, flags);
    value = observations;
    raw_spin_unlock_irqrestore(&observation_lock, flags);
    return value;
}
static void wake(void)
{
    if (READ_ONCE(ready) && READ_ONCE(enabled) && !READ_ONCE(stopping))
        mod_delayed_work(system_wq, &work, 0);
}
static void message(struct mms_subscribe *s, enum mms_msg_type type, u32 id, bool sync)
{
    if (type == MSG_TYPE_ITEM &&
        ((s == common_sub && id == LED_ITEM) || (s == wired_sub && id == 1))) wake();
}
static int power_event(struct notifier_block *nb, unsigned long action, void *data)
{
    wake();
    return NOTIFY_OK;
}
static struct notifier_block power_nb = { .notifier_call = power_event };

/* The checked descriptor belongs to the pinned common topic. No item state is
 * written here, and an existing debug override is never cleared or adopted. */
static int check_item(void)
{
    const u8 *desc, *table;
    unsigned count, i, matches = 0;
    u8 *found = NULL;
    if (!topics[0] || common_unbinding) return -ENODEV;
    desc = READ_ONCE(*(const u8 **)topics[0]);
    if (!desc) return -ENODEV;
    table = READ_ONCE(*(const u8 **)(desc + 16));
    count = READ_ONCE(*(const u32 *)(desc + 24));
    if (!table || !count || count > 256) return -ENOEXEC;
    for (i = 0; i < count; i++) {
        u8 *item = (u8 *)table + i * ITEM_SIZE;
        if (READ_ONCE(*(const u32 *)item) != LED_ITEM) continue;
        matches++;
        found = item;
    }
    if (matches != 1 || READ_ONCE(found[44]) ||
        READ_ONCE(*(void **)(found + 24)) != led_getter)
        return -ENOEXEC;
    /* No string/threshold/dead-zone flags: every explicit LED publication
     * must notify subscribers, including an unchanged value on restoration. */
    for (i = 4; i < 24; i += 4)
        if (READ_ONCE(*(const u32 *)(found + i))) return -ENOEXEC;
    led_item = found;
    return 0;
}
static int effective(int *value)
{
    union mms_msg_data data = {0};
    int rc;
    if (!topics[0] || common_unbinding) return -ENODEV;
    rc = cg_item(topics[0], LED_ITEM, &data, false);
    if (rc < 0) return rc;
    if (data.intval < 0 || data.intval > 1) return -ERANGE;
    *value = data.intval;
    return 0;
}
/* Process context only. The normal sync publisher refreshes the item under its
 * update mutex, then notifies consumers; success owns/frees the message. */
static int publish(bool covering)
{
    struct mms_msg *msg;
    unsigned long flags;
    u64 before;
    int rc, value;
    if (!topics[0] || common_unbinding || !led_item) return -ENODEV;
    before = observation_count();
    msg = cg_alloc(MSG_TYPE_ITEM, MSG_PRIO_MEDIUM, LED_ITEM);
    if (!msg) return -ENOMEM;
    rc = cg_publish(topics[0], msg);
    if (rc < 0) { kfree(msg); return rc; }
    publications++;
    if (observation_count() == before) return -EIO;
    /* After the shared update mutex has completed, the cache must contain an
     * ordinary bool. With active=false this was produced by the real getter.
     * Do not compare against a saved screen value: it can change concurrently. */
    value = READ_ONCE(*(const int *)(led_item + 96));
    if (value < 0 || value > 1 || (covering && value != 0)) return -EIO;
    rc = effective(&value);
    if (rc) return rc;
    if (covering && (value != 0 || READ_ONCE(led_item[112]))) return -EBUSY;
    raw_spin_lock_irqsave(&observation_lock, flags);
    effective_led = value;
    raw_spin_unlock_irqrestore(&observation_lock, flags);
    /* A foreign overwrite after activation is left intact. Restoring our own
     * getter/cache is sufficient; its effective value is reported separately. */
    return 0;
}
static void discharge_obligation(void)
{
    needs_restore = false;
    restore_scheduled = false;
    restore_attempts = 0;
    if (self_held) {
        self_held = false;
        module_put(THIS_MODULE);
    }
}
static int restore(void)
{
    int rc;
    set_active(false);
    if (!needs_restore) return 0;
    rc = publish(false);
    if (rc) {
        phase = "restore_pending";
        last_error = rc;
        /* Retry only this obligation, independently of enabled/session work.
         * Repeated off/close requests must not restart the finite budget or
         * postpone an already queued retry. Keep our reference on exhaustion. */
        if (!stopping && !common_unbinding && topics[0] && led_item &&
            restore_attempts < RESTORE_RETRIES && !restore_scheduled) {
            restore_scheduled = true;
            restore_due = jiffies + (HZ << restore_attempts);
            mod_delayed_work(system_wq, &restore_work, HZ << restore_attempts);
        }
        return rc;
    }
    discharge_obligation();
    return 0;
}
static int eligible(void)
{
    union power_supply_propval status;
    union mms_msg_data online = {0};
    int rc;
    if (power_supply_get_property(battery, POWER_SUPPLY_PROP_STATUS, &status) < 0)
        return -EIO;
    if (status.intval != POWER_SUPPLY_STATUS_CHARGING) return 0;
    if (!topics[1]) return -ENODEV;
    rc = cg_item(topics[1], 1, &online, false);
    if (rc < 0) return rc;
    if (online.intval < 0 || online.intval > 1) return -ERANGE;
    return online.intval;
}
static int fail(int error)
{
    int rc;
    enabled = false;
    rc = restore();
    if (rc) return rc;
    last_error = error;
    phase = error == -EBUSY ? "conflict" : "data_error";
    return error;
}
static int maintain_locked(void)
{
    int wanted, rc, value;
    if (!ready || retiring || stopping || !enabled) return 0;
    wanted = eligible();
    if (wanted < 0) return fail(wanted);
    if (!wanted) {
        rc = restore();
        if (rc) { enabled = false; return rc; }
        phase = "waiting_charge";
        last_error = 0;
        return 0;
    }
    rc = check_item();
    if (rc) return fail(rc);
    if (READ_ONCE(led_item[112])) return fail(-EBUSY);
    if (active) {
        rc = effective(&value);
        if (rc || value != 0) return fail(rc ? rc : -EBUSY);
        return 0;
    }
    if (!try_module_get(THIS_MODULE)) return fail(-ENODEV);
    self_held = needs_restore = true;
    set_active(true);
    rc = publish(true);
    if (rc) return fail(rc);
    phase = "applied";
    last_error = 0;
    return 0;
}
static void maintain(struct work_struct *unused)
{
    mutex_lock(&lock);
    maintain_locked();
    mutex_unlock(&lock);
}

static void release_wired(void)
{
    if (wired_sub) { cg_unsubscribe(wired_sub); wired_sub = NULL; }
    if (topics[1]) { cg_put(topics[1]); topics[1] = NULL; }
}
static void release_common(void)
{
    if (common_sub) { cg_unsubscribe(common_sub); common_sub = NULL; }
    if (topics[0]) { cg_put(topics[0]); topics[0] = NULL; }
    led_item = NULL;
}
static void retry_restore(struct work_struct *unused)
{
    unsigned long now;
    mutex_lock(&lock);
    now = jiffies;
    if (needs_restore && !active && !stopping && !common_unbinding && restore_scheduled &&
        restore_attempts < RESTORE_RETRIES) {
        /* A callback left over from an already discharged obligation may
         * enter after a new one was queued. Do not consume its budget early. */
        if (time_before(now, restore_due)) {
            mod_delayed_work(system_wq, &restore_work, restore_due - now);
            mutex_unlock(&lock);
            return;
        }
        restore_scheduled = false;
        restore_attempts++;
        if (!restore()) {
            phase = retiring ? "retired" : "off";
            last_error = 0;
            if (retiring) release_common();
        }
    }
    mutex_unlock(&lock);
}
static int bus_event(struct notifier_block *nb, unsigned long action, void *data)
{
    bool common = data == parents[0];
    if (!common && data != parents[1]) return NOTIFY_DONE;
    if (action == BUS_NOTIFY_UNBOUND_DRIVER) {
        mutex_lock(&lock);
        /* The exact common instance and its cache have been destroyed. There
         * is no remaining value to restore, even if its final publish failed. */
        if (common && common_unbinding) {
            discharge_obligation();
            phase = "retired";
            last_error = 0;
        }
        mutex_unlock(&lock);
        return NOTIFY_OK;
    }
    if (action != BUS_NOTIFY_UNBIND_DRIVER) return NOTIFY_DONE;
    mutex_lock(&lock);
    retiring = true;
    ready = enabled = false;
    restore(); /* Blocking bus notifier runs before driver remove/devres. */
    release_wired();
    if (common) {
        common_unbinding = true;
        release_common();
    } else if (!needs_restore) {
        release_common();
    }
    if (!needs_restore) { phase = "retired"; last_error = 0; }
    mutex_unlock(&lock);
    cancel_delayed_work_sync(&work);
    if (common) cancel_delayed_work_sync(&restore_work);
    return NOTIFY_OK;
}
static struct notifier_block bus_nb = { .notifier_call = bus_event };
static int reboot_event(struct notifier_block *nb, unsigned long action, void *data)
{
    mutex_lock(&lock);
    stopping = true;
    ready = enabled = false;
    restore();
    release_wired();
    if (!needs_restore) { release_common(); phase = "retired"; }
    mutex_unlock(&lock);
    cancel_delayed_work_sync(&work);
    cancel_delayed_work_sync(&restore_work);
    return NOTIFY_OK;
}
static struct notifier_block reboot_nb = { .notifier_call = reboot_event };

static int control_open(struct inode *inode, struct file *file)
{
    int rc = 0;
    mutex_lock(&lock);
    if (opened) rc = -EBUSY;
    else if ((!ready || retiring || stopping) && !needs_restore) rc = -ENODEV;
    else opened = true;
    mutex_unlock(&lock);
    return rc;
}
static ssize_t control_write(struct file *file, const char __user *buffer, size_t size, loff_t *pos)
{
    char text[3];
    int rc = 0;
    bool on;
    if (!size || size > 2) return -EINVAL;
    if (copy_from_user(text, buffer, size)) return -EFAULT;
    if ((text[0] != '0' && text[0] != '1') || (size == 2 && text[1] != '\n')) return -EINVAL;
    on = text[0] == '1';
    mutex_lock(&lock);
    if (!opened) rc = -EBADF;
    else if (!on) {
        enabled = false;
        rc = restore();
        if (!rc) { phase = retiring || stopping ? "retired" : "off"; last_error = 0; }
    } else if (!ready || retiring || stopping) rc = -ENODEV;
    else if (needs_restore && !active) rc = -EBUSY;
    else {
        enabled = true;
        rc = maintain_locked();
    }
    mutex_unlock(&lock);
    if (!on || rc) cancel_delayed_work_sync(&work);
    return rc ? rc : size;
}
static int control_release(struct inode *inode, struct file *file)
{
    mutex_lock(&lock);
    enabled = false;
    if (!restore()) { phase = retiring || stopping ? "retired" : "off"; last_error = 0; }
    mutex_unlock(&lock);
    cancel_delayed_work_sync(&work);
    mutex_lock(&lock);
    opened = false;
    mutex_unlock(&lock);
    return 0;
}
static const struct file_operations fops = {
    .owner = THIS_MODULE, .open = control_open, .write = control_write,
    .release = control_release, .llseek = no_llseek,
};
static struct miscdevice control = { .minor = MISC_DYNAMIC_MINOR,
    .name = "charge_guard_screen", .fops = &fops, .mode = 0600 };
static int get_status(char *buffer, const struct kernel_param *param)
{
    unsigned long flags;
    int n, original, adopted;
    u64 count;
    mutex_lock(&lock);
    raw_spin_lock_irqsave(&observation_lock, flags);
    original = original_led; adopted = effective_led; count = observations;
    raw_spin_unlock_irqrestore(&observation_lock, flags);
    n = snprintf(buffer, PAGE_SIZE,
        "api=1 driver=" DRIVER_ID " ready=%u enabled=%u session=%u active=%u pending=%u original_led=%d effective_led=%d observations=%llu publications=%llu state=%s error=%d\n",
        ready, enabled, opened, active, needs_restore && !active, original, adopted,
        (unsigned long long)count, (unsigned long long)publications, phase, last_error);
    mutex_unlock(&lock);
    return n;
}
static const struct kernel_param_ops status_ops = { .get = get_status };
module_param_cb(status, &status_ops, NULL, 0400);

static int resolve(void)
{
    void **dest[] = {(void **)&cg_topic, (void **)&cg_item, (void **)&cg_subscribe,
        (void **)&cg_unsubscribe, (void **)&cg_put, (void **)&cg_alloc,
        (void **)&cg_publish, &led_getter};
    unsigned i;
    for (i = 0; i < ARRAY_SIZE(cg_anchors); i++) {
        struct kprobe resolver = { .symbol_name = cg_anchors[i].symbol };
        u32 *address;
        int rc = register_kprobe(&resolver);
        if (rc) return rc;
        address = (u32 *)resolver.addr;
        unregister_kprobe(&resolver); /* Entry is original again, not our BRK. */
        if (READ_ONCE(address[-1]) != cg_anchors[i].kcfi ||
            READ_ONCE(address[0]) != cg_anchors[i].words[0] ||
            READ_ONCE(address[1]) != cg_anchors[i].words[1] ||
            READ_ONCE(address[2]) != cg_anchors[i].words[2]) return -ENOEXEC;
        *dest[i] = address;
    }
    for (i = 0; i < ARRAY_SIZE(cg_led_anchors); i++)
        if (READ_ONCE(*(const u32 *)((const u8 *)led_getter + cg_led_anchors[i].offset))
            != cg_led_anchors[i].word) return -ENOEXEC;
    return 0;
}
static int pin_driver(void)
{
    struct device_node *node = of_find_compatible_node(NULL, NULL, "oplus,common-charge");
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
            driver_owner = dev->driver->owner;
            rc = 0;
        }
        device_unlock(dev);
    }
    put_device(dev);
    return rc;
}
static int bind_topics(void)
{
    int i, rc = -ENODEV;
    for (i = 0; i < 2; i++) {
        topics[i] = cg_topic(topic_names[i]);
        if (!topics[i]) return -ENODEV;
        parents[i] = get_device(((struct device *)((u8 *)topics[i] + TOPIC_DEVICE_OFFSET))->parent);
        if (!parents[i] || !device_trylock(parents[i])) return -ENODEV;
        if (parents[i]->driver && parents[i]->driver->owner == driver_owner) rc = 0;
        else rc = -ENODEV;
        device_unlock(parents[i]);
        if (rc) return rc;
    }
    rc = check_item();
    if (rc) return rc;
    common_sub = cg_subscribe(topics[0], &control, message, "charge_guard_screen");
    if (IS_ERR(common_sub)) { rc = PTR_ERR(common_sub); common_sub = NULL; return rc; }
    wired_sub = cg_subscribe(topics[1], &control, message, "charge_guard_screen");
    if (IS_ERR(wired_sub)) { rc = PTR_ERR(wired_sub); wired_sub = NULL; return rc; }
    return 0;
}
static void release_resources(void)
{
    release_wired();
    release_common();
    if (hook_registered) { unregister_kprobe(&led_probe); hook_registered = false; }
    if (battery) { power_supply_put(battery); battery = NULL; }
    if (parents[0]) { put_device(parents[0]); parents[0] = NULL; }
    if (parents[1]) { put_device(parents[1]); parents[1] = NULL; }
    if (driver_owner) { module_put(driver_owner); driver_owner = NULL; }
}
static int __init cg_screen_init(void)
{
    int rc;
    INIT_DELAYED_WORK(&work, maintain);
    INIT_DELAYED_WORK(&restore_work, retry_restore);
    rc = bus_register_notifier(&platform_bus_type, &bus_nb);
    if (rc) return rc;
    mutex_lock(&lock);
    rc = pin_driver();
    if (!rc) rc = resolve();
    if (!rc) rc = bind_topics();
    if (!rc) {
        led_probe.addr = (kprobe_opcode_t *)((u8 *)led_getter + CG_LED_PROBE_OFFSET);
        led_probe.pre_handler = led_pre;
        rc = register_kprobe(&led_probe);
        if (!rc) hook_registered = true;
    }
    if (!rc) {
        battery = power_supply_get_by_name("battery");
        if (!battery) rc = -ENODEV;
    }
    mutex_unlock(&lock);
    if (rc) goto fail;
    rc = power_supply_reg_notifier(&power_nb);
    if (rc) goto fail;
    rc = register_reboot_notifier(&reboot_nb);
    if (rc) goto fail_power;
    rc = misc_register(&control);
    if (rc) goto fail_reboot;
    mutex_lock(&lock);
    rc = retiring ? -ENODEV : 0;
    if (!rc) ready = true;
    mutex_unlock(&lock);
    if (!rc) return 0;
    misc_deregister(&control);
fail_reboot:
    unregister_reboot_notifier(&reboot_nb);
fail_power:
    power_supply_unreg_notifier(&power_nb);
fail:
    bus_unregister_notifier(&platform_bus_type, &bus_nb);
    release_resources();
    return rc;
}
static void __exit cg_screen_exit(void)
{
    WRITE_ONCE(stopping, true);
    misc_deregister(&control);
    unregister_reboot_notifier(&reboot_nb);
    power_supply_unreg_notifier(&power_nb);
    bus_unregister_notifier(&platform_bus_type, &bus_nb);
    cancel_delayed_work_sync(&work);
    cancel_delayed_work_sync(&restore_work);
    mutex_lock(&lock);
    ready = enabled = false;
    set_active(false);
    /* A pending restoration holds our own module reference, so ordinary
     * unload can only arrive here after the real state has been republished. */
    release_resources();
    mutex_unlock(&lock);
}
module_init(cg_screen_init);
module_exit(cg_screen_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Shared charging screen-off judgment with exclusive reversible control");
