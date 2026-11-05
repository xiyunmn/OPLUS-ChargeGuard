/* SPDX-License-Identifier: GPL-2.0-only */
/* Process-context CPA re-admission only. No worker, OEM vote or PDO writes. */
#include <linux/mutex.h>
#include <linux/of.h>
#include <linux/platform_device.h>
#include <linux/power_supply.h>
#include "pps_recovery_anchors.h"

struct oplus_mms;
struct oplus_pps;
struct votable;
union mms_msg_data { int intval; char *strval; };
enum oplus_chg_protocol_type {
    CHG_PROTOCOL_QC, CHG_PROTOCOL_PD, CHG_PROTOCOL_PPS,
    CHG_PROTOCOL_VOOC, CHG_PROTOCOL_UFCS, CHG_PROTOCOL_BC12,
};
static struct oplus_mms *(*rec_topic)(const char *);
static int (*rec_item)(struct oplus_mms *, u32, union mms_msg_data *, bool);
static void (*rec_put)(struct oplus_mms *);
static void *(*rec_drvdata)(struct oplus_mms *);
static struct votable *(*rec_find)(const char *);
static int (*rec_effective)(struct votable *);
static int (*rec_request)(struct oplus_mms *, enum oplus_chg_protocol_type);
static bool (*rec_check_enable)(struct oplus_mms *, enum oplus_chg_protocol_type);

/* Also held by enabled writes: after enabled=0 returns, this helper has no
 * in-flight recovery action. A request already accepted by OEM belongs to OEM. */
static DEFINE_MUTEX(rec_lock);
static struct module *rec_owner;
static bool rec_ready;
static int rec_init_error;
static struct {
    const char *state, *blocking_voter;
    int error, temperature_decic, real_type, protocol;
    bool ready;
    u32 attempts, successes;
    u64 checked_ms, last_request_ms;
} rec = { .state = "off", .blocking_voter = "none",
    .temperature_decic = -10000, .real_type = -1, .protocol = -1 };
static struct {
    bool ever_charging, cool_latched, cool_opportunity, awaiting, stable_reset;
    int request_error;
    u64 exit_ms, active_since_ms, cable_since_ms;
} rec_session;

enum rec_topic_id { RT_PPS, RT_CPA, RT_WIRED, RT_COMMON, RT_GAUGE, RT_VOOC, RT_UFCS, RT_COUNT };
static const char *const rec_topic_names[] = { "pps", "cpa", "wired", "common", "gauge", "vooc", "ufcs" };
static const char *const rec_vote_names[] = {
    "PPS_DISABLE", "PPS_NOT_ALLOW", "PPS_BOOT",
    "WIRED_CHARGING_DISABLE", "WIRED_CHARGE_SUSPEND",
};
struct rec_context {
    struct oplus_mms *topics[RT_COUNT];
    struct device *parents[RT_COUNT];
    unsigned parent_count, locked;
    struct power_supply *battery;
};

static void rec_result(const char *state, int error, const char *blocker)
{
    rec.state = state; rec.error = error;
    rec.blocking_voter = blocker ? blocker : "none";
}
/* rec_lock held, never called from a kprobe. */
static void rec_reset_session(bool on)
{
    memset(&rec_session, 0, sizeof(rec_session));
    rec.attempts = rec.successes = 0;
    rec.checked_ms = rec.last_request_ms = 0;
    rec.temperature_decic = -10000; rec.real_type = rec.protocol = -1;
    rec.ready = rec_ready;
    rec_result(!rec_ready ? "unsupported" : on ? "observing" : "off",
               rec_ready ? 0 : rec_init_error, NULL);
}
static int rec_pin_driver(void)
{
    struct device_node *node = of_find_compatible_node(NULL, NULL, "oplus,pps_charge");
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
            rec_owner = dev->driver->owner;
            rc = 0;
        }
        device_unlock(dev);
    }
    put_device(dev);
    return rc;
}
static int rec_resolve(void)
{
    void **dest[] = { (void **)&rec_topic, (void **)&rec_item,
        (void **)&rec_put, (void **)&rec_drvdata, (void **)&rec_find,
        (void **)&rec_effective, (void **)&rec_request,
        (void **)&rec_check_enable };
    unsigned i;
    if (ARRAY_SIZE(dest) != ARRAY_SIZE(rec_anchors)) return -ENOEXEC;
    for (i = 0; i < ARRAY_SIZE(rec_anchors); i++) {
        struct kprobe lookup = { .symbol_name = rec_anchors[i].symbol };
        const u32 *address;
        int rc = register_kprobe(&lookup);
        if (rc) return rc;
        address = (const u32 *)lookup.addr;
        unregister_kprobe(&lookup);
        if (READ_ONCE(address[-1]) != rec_anchors[i].kcfi ||
            READ_ONCE(address[0]) != rec_anchors[i].words[0] ||
            READ_ONCE(address[1]) != rec_anchors[i].words[1] ||
            READ_ONCE(address[2]) != rec_anchors[i].words[2]) return -ENOEXEC;
        *dest[i] = (void *)address;
    }
    for (i = 0; i < ARRAY_SIZE(rec_layout_anchors); i++) {
        struct kprobe lookup = { .symbol_name = rec_layout_anchors[i].symbol };
        const u8 *address;
        int rc = register_kprobe(&lookup);
        if (rc) return rc;
        address = (const u8 *)lookup.addr;
        unregister_kprobe(&lookup);
        if (READ_ONCE(*(const u32 *)(address + rec_layout_anchors[i].offset)) !=
            rec_layout_anchors[i].word) return -ENOEXEC;
    }
    return 0;
}
static void rec_init(void)
{
    int rc = rec_pin_driver();
    if (!rc) rc = rec_resolve();
    rec_init_error = rc;
    rec_ready = !rc;
    if (rc && rec_owner) { module_put(rec_owner); rec_owner = NULL; }
    rec_reset_session(false);
}
static void rec_cleanup(void)
{
    mutex_lock(&rec_lock);
    rec_ready = rec.ready = false;
    rec_result("retired", -ENODEV, NULL);
    if (rec_owner) { module_put(rec_owner); rec_owner = NULL; }
    mutex_unlock(&rec_lock);
}
static void rec_release(struct rec_context *ctx)
{
    unsigned i;
    if (ctx->battery) power_supply_put(ctx->battery);
    while (ctx->locked) device_unlock(ctx->parents[--ctx->locked]);
    while (ctx->parent_count) put_device(ctx->parents[--ctx->parent_count]);
    for (i = RT_COUNT; i > 0; i--)
        if (ctx->topics[i - 1]) rec_put(ctx->topics[i - 1]);
}
static int rec_acquire(struct rec_context *ctx)
{
    unsigned i, j;
    for (i = 0; i < RT_COUNT; i++) {
        struct device *parent;
        ctx->topics[i] = rec_topic(rec_topic_names[i]);
        if (!ctx->topics[i]) return -ENODEV;
        parent = ((struct device *)((u8 *)ctx->topics[i] + 488))->parent;
        if (!parent) return -ENODEV;
        for (j = 0; j < ctx->parent_count; j++)
            if (ctx->parents[j] == parent) break;
        if (j < ctx->parent_count) continue;
        parent = get_device(parent);
        if (!parent) return -ENODEV;
        ctx->parents[ctx->parent_count++] = parent;
        if (!device_trylock(parent)) return -EBUSY;
        ctx->locked++;
        if (!parent->driver || parent->driver->owner != rec_owner) return -ENODEV;
    }
    ctx->battery = power_supply_get_by_name("battery");
    return ctx->battery ? 0 : -ENODEV;
}
static int rec_read(struct rec_context *ctx, unsigned topic, u32 id, int *value)
{
    union mms_msg_data data = { 0 };
    int rc = rec_item(ctx->topics[topic], id, &data, true);
    if (rc < 0) return rc;
    *value = data.intval;
    return 0;
}
static int rec_protections(void)
{
    unsigned i;
    for (i = 0; i < ARRAY_SIZE(rec_vote_names); i++) {
        struct votable *v = rec_find(rec_vote_names[i]);
        int value;
        if (!v) { rec.blocking_voter = rec_vote_names[i]; return -ENODEV; }
        value = rec_effective(v);
        if (value != 0 && value != -EINVAL) {
            rec.blocking_voter = rec_vote_names[i];
            return value > 0 ? -EBUSY : value;
        }
    }
    return 0;
}
/* Parent-device locks keep every OEM consumer alive; state_lock only checks
 * helper identity. No OEM API is called under the raw spinlock. */
static bool rec_bound(struct oplus_pps *chip, bool *ever, u64 *exit_ms, u64 *interruption_ms)
{
    unsigned long flags;
    bool valid;
    raw_spin_lock_irqsave(&state_lock, flags);
    valid = ready && enabled && !retiring && bound_chip && (void *)bound_chip == chip;
    if (ever) *ever = diag.ever_charging &&
        diag.last_charging_ms >= rec_session.cable_since_ms;
    if (exit_ms) {
        *exit_ms = max(diag.exit_ms, diag.last_reset_ms);
        if (*exit_ms < diag.last_charging_ms) *exit_ms = 0;
    }
    if (interruption_ms) *interruption_ms = max(diag.exit_ms, diag.last_reset_ms);
    raw_spin_unlock_irqrestore(&state_lock, flags);
    return valid;
}
static bool rec_final_conditions(struct rec_context *ctx, struct oplus_pps *chip)
{
    union power_supply_propval prop;
    int rc, wired, cc, real_type, charging, vooc, ufcs, allow;
    int soc, low_soc, high_soc;
    rc = rec_read(ctx, RT_WIRED, 1, &wired);
    if (!rc) rc = rec_read(ctx, RT_WIRED, 6, &cc);
    if (!rc) rc = rec_read(ctx, RT_WIRED, 19, &real_type);
    if (!rc) rc = rec_read(ctx, RT_PPS, 1, &charging);
    if (!rc) rc = rec_read(ctx, RT_VOOC, 2, &vooc);
    if (!rc) rc = rec_read(ctx, RT_UFCS, 1, &ufcs);
    if (!rc) rc = rec_read(ctx, RT_CPA, 1, &allow);
    if (rc) { rec_result("waiting_conditions", rc, "MMS"); return false; }
    rec.real_type = real_type;
    if (cc < 0 || cc > 2) {
        rec_result("waiting_conditions", -ERANGE, "CC_DETECT"); return false;
    }
    if (!wired || cc == 1 || real_type != 8 || charging || vooc || ufcs) {
        rec_result("waiting_conditions", 0, "PROTOCOL"); return false;
    }
    rc = power_supply_get_property(ctx->battery, POWER_SUPPLY_PROP_STATUS, &prop);
    if (rc || prop.intval != POWER_SUPPLY_STATUS_CHARGING) {
        rec.protocol = rc ? -1 : 0;
        rec_result("waiting_conditions", rc, "BATTERY_STATUS"); return false;
    }
    rc = power_supply_get_property(ctx->battery, POWER_SUPPLY_PROP_HEALTH, &prop);
    if (rc || prop.intval != POWER_SUPPLY_HEALTH_GOOD) {
        rec_result("waiting_conditions", rc, "BATTERY_HEALTH"); return false;
    }
    rc = power_supply_get_property(ctx->battery, POWER_SUPPLY_PROP_TEMP, &prop);
    if (rc || prop.intval < 0 || prop.intval > 1000) {
        rec.temperature_decic = -10000;
        rec_result("waiting_conditions", rc ? rc : -ERANGE, "TEMPERATURE"); return false;
    }
    rec.temperature_decic = prop.intval;
    if (rec.temperature_decic >= 450) rec_session.cool_latched = true;
    if (rec_session.cool_latched) {
        rec_result("waiting_cool", 0, "TEMPERATURE"); return false;
    }
    soc = READ_ONCE(*(int *)((u8 *)chip + 3004));
    low_soc = READ_ONCE(*(int *)((u8 *)chip + 2440));
    high_soc = READ_ONCE(*(int *)((u8 *)chip + 2464));
    if (soc < low_soc || soc > high_soc || soc < 0 || soc > 100) {
        rec_result("waiting_conditions", 0, "SOC"); return false;
    }
    if ((allow != CHG_PROTOCOL_PD && allow != -1) ||
        !rec_check_enable(ctx->topics[RT_CPA], CHG_PROTOCOL_PPS)) {
        rec_result("waiting_conditions", 0, "CPA"); return false;
    }
    return true;
}
static void rec_maintain(void)
{
    struct rec_context ctx = { 0 };
    struct oplus_pps *chip;
    union power_supply_propval prop;
    u64 now = diag_now(), event_ms = 0, interruption_ms = 0, previous_ms = rec.checked_ms;
    bool is_enabled, is_ready, is_retiring, ever = false;
    unsigned long flags;
    int rc, wired, cc_detect, charging, vooc, ufcs, type, allow, soc, low_soc, high_soc;

    rec.checked_ms = now;
    rec.temperature_decic = -10000; rec.real_type = rec.protocol = -1;
    rec.blocking_voter = "none"; rec.error = 0;
    raw_spin_lock_irqsave(&state_lock, flags);
    is_enabled = enabled; is_ready = ready; is_retiring = retiring;
    raw_spin_unlock_irqrestore(&state_lock, flags);
    rec.ready = rec_ready && is_ready && !is_retiring;
    if (is_retiring) { rec_result("retired", -ENODEV, NULL); return; }
    if (!rec_ready) { rec_result("unsupported", rec_init_error, NULL); return; }
    if (!is_enabled) { rec_result("off", 0, NULL); return; }
    if (!is_ready) { rec_result("waiting_conditions", -ENODEV, "HELPER"); return; }

    rc = rec_acquire(&ctx);
    if (rc) { rec_result("waiting_conditions", rc, "TOPIC"); goto invalid; }
    chip = rec_drvdata(ctx.topics[RT_PPS]);
    if (!rec_bound(chip, &ever, &event_ms, &interruption_ms)) {
        rec_result("observing", -EAGAIN, "HELPER"); goto invalid;
    }
    rc = rec_read(&ctx, RT_WIRED, 1, &wired);
    if (!rc) rc = rec_read(&ctx, RT_WIRED, 6, &cc_detect);
    if (!rc) rc = rec_read(&ctx, RT_WIRED, 19, &rec.real_type);
    if (!rc) rc = rec_read(&ctx, RT_WIRED, 3, &type);
    if (!rc) rc = rec_read(&ctx, RT_PPS, 1, &charging);
    if (!rc) rc = rec_read(&ctx, RT_VOOC, 2, &vooc);
    if (!rc) rc = rec_read(&ctx, RT_UFCS, 1, &ufcs);
    if (rc) { rec_result("waiting_conditions", rc, "MMS"); goto invalid; }
    if (cc_detect < 0 || cc_detect > 2) {
        rec_result("waiting_conditions", -ERANGE, "CC_DETECT"); goto invalid;
    }
    if (cc_detect == 1) {
        /* CC_DETECT_NOTPLUG confirms a real cable removal. HardReset may only
         * drop wired online briefly; that must retain the recovery history. */
        rec_reset_session(true); rec.checked_ms = now;
        rec_session.cable_since_ms = now;
        rec.protocol = 0; rec.real_type = -1;
        rec_result("observing", 0, "WIRED_OFFLINE"); goto out;
    }
    if (!wired) {
        rec_result("waiting_conditions", 0, "WIRED_OFFLINE"); goto invalid;
    }
    if (vooc || ufcs || (type != 6 && type != 7 && type != 8)) rec.protocol = 0;
    else rec.protocol = charging ? 3 : 4;
    rc = power_supply_get_property(ctx.battery, POWER_SUPPLY_PROP_TEMP, &prop);
    if (rc) { rec_result("waiting_conditions", rc, "TEMPERATURE"); goto invalid; }
    rec.temperature_decic = prop.intval;
    if (rec.temperature_decic < 0 || rec.temperature_decic > 1000) {
        rec.temperature_decic = -10000;
        rec_result("waiting_conditions", -ERANGE, "TEMPERATURE"); goto invalid;
    }
    if (rec.temperature_decic >= 450) rec_session.cool_latched = true;
    else if (rec_session.cool_latched && rec.temperature_decic <= 440) {
        rec_session.cool_latched = false;
        rec_session.cool_opportunity = true;
    }
    /* A diagnostic edge can establish the earlier PPS session even when the
     * 2-3 second userspace maintenance did not sample its charging interval. */
    rec_session.ever_charging |= ever || (charging && rec.protocol == 3);
    rc = power_supply_get_property(ctx.battery, POWER_SUPPLY_PROP_STATUS, &prop);
    if (rc || prop.intval != POWER_SUPPLY_STATUS_CHARGING) {
        rec.protocol = rc ? -1 : 0;
        rec_result("waiting_conditions", rc, "BATTERY_STATUS"); goto invalid;
    }
    if (charging && rec.protocol == 3) {
        bool recovered = rec_session.awaiting;
        if (recovered) { rec.successes++; rec_session.awaiting = false; }
        if (rec_session.cool_opportunity) {
            rec.attempts = 0;
            rec_session.cool_opportunity = false;
        }
        if (!rec_session.active_since_ms || !previous_ms || now - previous_ms > 6000 ||
            interruption_ms > rec_session.active_since_ms) {
            rec_session.active_since_ms = now;
            rec_session.stable_reset = false;
        }
        if (!rec_session.stable_reset && now - rec_session.active_since_ms >= 60000) {
            rec.attempts = 0;
            rec_session.stable_reset = true;
        }
        rec_session.exit_ms = 0;
        rec_result(recovered ? "recovered" : "active", 0, NULL); goto out;
    }
    rec_session.active_since_ms = 0; rec_session.stable_reset = false;
    if (!rec_session.ever_charging) { rec_result("observing", 0, NULL); goto out; }
    if (!rec_session.exit_ms)
        rec_session.exit_ms = event_ms && event_ms <= now &&
            event_ms >= rec_session.cable_since_ms ? event_ms : now;
    if (event_ms > rec_session.exit_ms && event_ms <= now) rec_session.exit_ms = event_ms;
    if (rec_session.cool_latched) { rec_result("waiting_cool", 0, "TEMPERATURE"); goto out; }
    if (charging || vooc || ufcs || rec.real_type != 8) {
        rec_result("waiting_conditions", 0, "PROTOCOL"); goto out;
    }
    rc = power_supply_get_property(ctx.battery, POWER_SUPPLY_PROP_HEALTH, &prop);
    if (rc || prop.intval != POWER_SUPPLY_HEALTH_GOOD) {
        rec_result("waiting_conditions", rc, "BATTERY_HEALTH"); goto out;
    }
    rc = rec_read(&ctx, RT_CPA, 1, &allow);
    if (rc || (allow != CHG_PROTOCOL_PD && allow != -1) ||
        !rec_check_enable(ctx.topics[RT_CPA], CHG_PROTOCOL_PPS)) {
        rec_result("waiting_conditions", rc, "CPA"); goto out;
    }
    soc = READ_ONCE(*(int *)((u8 *)chip + 3004));
    low_soc = READ_ONCE(*(int *)((u8 *)chip + 2440));
    high_soc = READ_ONCE(*(int *)((u8 *)chip + 2464));
    if (soc < low_soc || soc > high_soc || soc < 0 || soc > 100) {
        rec_result("waiting_conditions", 0, "SOC"); goto out;
    }
    rc = rec_protections();
    if (rc) { rec_result("waiting_conditions", rc, rec.blocking_voter); goto out; }
    if (rec_session.cool_opportunity) {
        rec.attempts = 0;
        rec_session.cool_opportunity = false;
    }
    if (now - rec_session.exit_ms < 5000) { rec_result("settling", 0, NULL); goto out; }
    if (rec.last_request_ms && now - rec.last_request_ms < 30000) {
        rec_result(rec_session.request_error ? "request_failed" : "requesting",
                   rec_session.request_error, NULL); goto out;
    }
    if (rec.attempts >= 2) {
        rec_result("retry_exhausted", 0, NULL); goto out;
    }
    /* The private allow function has no KCFI entry and must only be called by
     * OEM's normal direct-call path. Recheck our admission conditions before
     * CPA; never rely on switch_check resetting a blocked protection vote. */
    if (!rec_final_conditions(&ctx, chip)) goto out;
    rc = rec_protections();
    if (rc) { rec_result("waiting_conditions", rc, rec.blocking_voter); goto out; }
    if (!rec_bound(chip, NULL, NULL, NULL)) { rec_result("retired", -ENODEV, NULL); goto out; }
    rec.attempts++; rec.last_request_ms = now;
    rc = rec_request(ctx.topics[RT_CPA], CHG_PROTOCOL_PPS);
    rec_session.awaiting |= !rc;
    rec_session.request_error = rc;
    rec_result(rc ? "request_failed" : "requesting", rc, NULL);
    goto out;
invalid:
    rec_session.active_since_ms = 0;
out:
    rec_release(&ctx);
}
static int set_recovery(const char *value, const struct kernel_param *param)
{
    (void)param;
    if (!value || value[0] != '1' ||
        (value[1] != '\0' && !(value[1] == '\n' && value[2] == '\0'))) return -EINVAL;
    mutex_lock(&rec_lock);
    rec_maintain();
    mutex_unlock(&rec_lock);
    return 0; /* Phase/error report optional recovery failure independently. */
}
static int get_recovery(char *buffer, const struct kernel_param *param)
{
    int n;
    (void)param;
    mutex_lock(&rec_lock);
    n = snprintf(buffer, PAGE_SIZE,
        "{\"api\":1,\"ready\":%s,\"state\":\"%s\",\"error\":%d,\"checked_ms\":%llu,"
        "\"temperature_decic\":%d,\"real_type\":%d,\"protocol\":%d,\"attempts\":%u,"
        "\"successes\":%u,\"last_request_ms\":%llu,\"blocking_voter\":\"%s\"}\n",
        rec.ready ? "true" : "false", rec.state, rec.error, rec.checked_ms,
        rec.temperature_decic, rec.real_type, rec.protocol, rec.attempts,
        rec.successes, rec.last_request_ms, rec.blocking_voter);
    mutex_unlock(&rec_lock);
    return n;
}
static const struct kernel_param_ops recovery_ops = { .set = set_recovery, .get = get_recovery };
module_param_cb(recovery, &recovery_ops, NULL, 0600);
