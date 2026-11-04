/* SPDX-License-Identifier: GPL-2.0-only */
/* Read-only observations of the OEM paths. Included by charge_guard_pps.c after
 * its state/lock declarations; never calls OEM APIs or alters registers/data. */
#include <linux/ktime.h>

#define DIAG_EVENTS 16
#define DIAG_VOTER 32
#define OEM_STATUS_INFO 2832
#define OEM_CHARGING 2871
#define OEM_CP_IBUS 2992
#define OEM_SUPPORT 2993

enum diag_kind {
    DIAG_BOUND = 1, DIAG_QUERY, DIAG_SUPPORT, DIAG_USED, DIAG_CHARGING,
    DIAG_DISABLE, DIAG_NOT_ALLOW, DIAG_SOFT_EXIT, DIAG_FORCE_EXIT,
    DIAG_HARD_RESET, DIAG_ENABLED, DIAG_RETIRING,
};
enum diag_point {
    DP_START_QUERY, DP_STEADY_QUERY, DP_USED, DP_REJECT_STEADY, DP_SUPPORT_START,
    DP_CHARGING, DP_DISABLE, DP_NOT_ALLOW, DP_SOFT_EXIT, DP_FORCE_EXIT, DP_PD,
};
struct diag_event {
    u64 seq, ms;
    unsigned kind, phase;
    s32 rc, support, current_ma;
    s64 raw;
    char voter[DIAG_VOTER];
};
static struct {
    bool ready, charging, session_used, raw_valid;
    int error, support, cp_ibus, rc;
    unsigned phase, exit;
    u32 raw;
    u64 generation_ms, seq, queries, successes, failures, uses, resets;
    u64 query_ms, used_ms;
    char disable_vote[DIAG_VOTER], not_allow_vote[DIAG_VOTER], exit_voter[DIAG_VOTER];
    struct diag_event events[DIAG_EVENTS];
} diag = { .support = -1, .cp_ibus = -1,
    .disable_vote = "none", .not_allow_vote = "none", .exit_voter = "none" };

static u64 diag_now(void) { return ktime_to_ms(ktime_get_boottime()); }
static void diag_voter(char *dst, const char *src)
{
    unsigned i;
    if (!src) src = "unknown";
    for (i = 0; i < DIAG_VOTER - 1; i++) {
        char c = READ_ONCE(src[i]);
        if (!c) break;
        /* OEM voter labels are ASCII identifiers. Keep the JSON bounded and
         * valid even if a new label contains punctuation. Never export pointers. */
        dst[i] = ((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') ||
                  (c >= '0' && c <= '9') || c == '_') ? c : '_';
    }
    dst[i] = 0;
}
/* state_lock held by all producers and readers. */
static void diag_emit(unsigned kind, unsigned phase, int rc, int current_ma, const char *voter)
{
    struct diag_event *e = &diag.events[diag.seq % DIAG_EVENTS];
    e->seq = ++diag.seq; e->ms = diag_now();
    e->kind = kind; e->phase = phase; e->rc = rc;
    e->raw = diag.raw_valid ? (s64)diag.raw : -1;
    e->support = diag.support; e->current_ma = current_ma;
    diag_voter(e->voter, voter ? voter : "none");
}
static void diag_support(int value, unsigned phase)
{
    bool known = diag.support >= 0;
    if (diag.support == value) return;
    diag.support = value;
    if (!value) diag.session_used = false;
    /* An initial zero before OEM initialization is not a data-channel failure.
     * Its value is included in BOUND; SUPPORT events describe observed changes. */
    if (known) diag_emit(DIAG_SUPPORT, phase, value, 0, NULL);
}
static void diag_charging(int value)
{
    if (diag.charging == !!value) return;
    diag.charging = !!value;
    diag.session_used = false;
    if (value) {
        diag.exit = 0;
        diag_voter(diag.exit_voter, "none");
    }
    diag_emit(DIAG_CHARGING, 0, !!value, 0, NULL);
}
static void diag_sync_chip(const u8 *chip)
{
    int support, cp, charging;
    if (!diag.ready) return;
    support = READ_ONCE(chip[OEM_SUPPORT]);
    cp = READ_ONCE(chip[OEM_CP_IBUS]);
    charging = READ_ONCE(chip[OEM_CHARGING]);
    if (support > 1 || cp > 1 || charging > 1) {
        diag.ready = false; diag.error = -EINVAL;
        return;
    }
    diag.cp_ibus = cp;
    diag_support(support, 0);
    diag_charging(charging);
}
static void diag_bound(const u8 *chip, bool first)
{
    if (!diag.ready) return;
    diag_sync_chip(chip);
    if (first) diag_emit(DIAG_BOUND, 0, 0, 0, NULL);
}
static void diag_query(const u8 *chip, unsigned phase, int rc)
{
    bool changed = !diag.queries || diag.rc != rc || diag.phase != phase;
    diag.queries++; diag.phase = phase; diag.rc = rc; diag.query_ms = diag_now();
    diag.raw_valid = rc >= 0;
    if (rc >= 0) {
        diag.successes++;
        diag.raw = READ_ONCE(*(const u32 *)(chip + OEM_STATUS_INFO));
        if (phase == 2 && (!(diag.raw & 0xff0000) || ((diag.raw >> 16) & 255) >= 140))
            diag.session_used = false;
    } else {
        diag.failures++;
        diag.raw = 0;
        diag.session_used = false;
    }
    if (changed) diag_emit(DIAG_QUERY, phase, rc, 0, NULL);
}
static void diag_vote(unsigned kind, int value, const char *voter)
{
    char label[DIAG_VOTER];
    char *saved = kind == DIAG_DISABLE ? diag.disable_vote : diag.not_allow_vote;
    diag_voter(label, value > 0 ? voter : "none");
    if (!strcmp(saved, label)) return;
    diag_voter(saved, label);
    diag_emit(kind, 0, value > 0, 0, saved);
}

struct diag_anchor { unsigned offset; u32 word; };
struct diag_hook {
    const char *symbol;
    unsigned offset;
    u32 instruction;
    struct diag_anchor anchors[2];
};
/* Sample b04f1153…; monitor/third-party logic also matches the original 205fb7…
 * interface contract. Each point is relative to its own qualified symbol.
 * Register meanings are checked at the actual site, including inlined queries. */
static const struct diag_hook diag_hooks[] = {
    { "oplus_chg_v2:oplus_pps_monitor_work", 0x29cc, 0x37f87ac0,
        {{0x29c0,0x91260261},{0x29c4,0xaa1403e0}} },
    { "oplus_chg_v2:oplus_pps_monitor_work", 0x3218, 0x37f80620,
        {{0x3214,0xd63f0100},{0x321c,0x39660a68}} },
    { "oplus_chg_v2:oplus_third_pps_target_voltage_check", 0x20c, 0xb81f43b5,
        {{0x204,0x6b0402bf},{0x208,0x1a84c2b5}} },
    { "oplus_chg_v2:oplus_pps_monitor_work", 0x3344, 0x3928867f,
        {{0x321c,0x39660a68},{0x322c,0x7102311f}} },
    { "oplus_chg_v2:oplus_pps_monitor_work", 0x3934, 0x39288668,
        {{0x29a8,0x396a5268},{0x29b8,0x39688668}} },
    { "oplus_chg_v2:oplus_pps_set_charging", 0x38, 0x7100113f,
        /* lookup arms the entry with BRK. Verify the chip-register setup and
         * original field store, which remain intact while lookup is registered. */
        {{0x2c,0xaa0003f3},{0x34,0x392cdc08}} },
    { "oplus_chg_v2:oplus_pps_disable_vote_callback", 0x10, 0x392d6429,
        {{0x4,0x7100005f},{0xc,0x1a9fd7e9}} },
    { "oplus_chg_v2:oplus_pps_not_allow_vote_callback", 0x38, 0x392d6828,
        {{0x20,0x7100005f},{0x30,0x1a9fd7e8}} },
    { "oplus_chg_v2:oplus_pps_soft_exit", 0x18, 0x392cd008,
        {{0x10,0xaa0003f3},{0x38,0x2a1f03e1}} },
    { "oplus_chg_v2:oplus_pps_force_exit", 0x18, 0x392cd008,
        {{0x10,0xaa0003f3},{0x38,0x2a1f03e1}} },
    { "oplus_chg_v2:battery_chg_callback", 0x4c, 0xb9400a88,
        {{0x28,0xaa0203f5},{0x30,0xaa0103f4}} },
};
static struct kprobe diag_probes[ARRAY_SIZE(diag_hooks)];
static unsigned diag_registered;

static int diag_pre(struct kprobe *probe, struct pt_regs *regs)
{
    unsigned long flags;
    unsigned point = probe - diag_probes;
    u8 *chip;
    raw_spin_lock_irqsave(&state_lock, flags);
    if (!ready || !enabled || retiring || !diag.ready) goto out;
    if (point == DP_PD) {
        const u32 *msg = (const u32 *)regs->regs[20];
        /* OEM callback: x20=message, x21=length; opcode=7 notification,
         * payload=0x7f Hard Reset. No inference about which side sent it. */
        if (regs->regs[21] == 16 && msg && READ_ONCE(msg[2]) == 7 && READ_ONCE(msg[3]) == 0x7f) {
            diag.resets++; diag.session_used = false;
            diag_emit(DIAG_HARD_RESET, 0, 0, 0, NULL);
        }
        goto out;
    }
    switch (point) {
    case DP_START_QUERY: case DP_STEADY_QUERY: case DP_REJECT_STEADY: case DP_SUPPORT_START:
        chip = (u8 *)regs->regs[19] - MONITOR_WORK_OFFSET; break;
    case DP_DISABLE: case DP_NOT_ALLOW: chip = (u8 *)regs->regs[1]; break;
    case DP_SOFT_EXIT: case DP_FORCE_EXIT: chip = (u8 *)regs->regs[0]; break;
    default: chip = (u8 *)regs->regs[19]; break;
    }
    if (!bound_chip || chip != bound_chip) goto out;
    switch (point) {
    case DP_START_QUERY: case DP_STEADY_QUERY:
        diag_query(chip, point == DP_START_QUERY ? 1 : 2, (s32)regs->regs[0]); break;
    case DP_USED:
        diag.uses++; diag.used_ms = diag_now();
        if (!diag.session_used) {
            diag.session_used = true;
            diag_emit(DIAG_USED, 0, 0, (s32)regs->regs[21], NULL);
        }
        break;
    case DP_REJECT_STEADY: diag_support(0, 2); break;
    case DP_SUPPORT_START: diag_support((u32)regs->regs[8], 1); break;
    case DP_CHARGING: diag_charging(READ_ONCE(chip[OEM_CHARGING])); break;
    case DP_DISABLE: case DP_NOT_ALLOW:
        diag_vote(point == DP_DISABLE ? DIAG_DISABLE : DIAG_NOT_ALLOW,
                  (s32)regs->regs[2], (const char *)regs->regs[3]); break;
    case DP_SOFT_EXIT: case DP_FORCE_EXIT:
        diag.exit = point == DP_SOFT_EXIT ? 1 : 2;
        diag.session_used = false;
        diag_voter(diag.exit_voter, strcmp(diag.disable_vote, "none") ? diag.disable_vote :
                   strcmp(diag.not_allow_vote, "none") ? diag.not_allow_vote : "unknown");
        diag_emit(point == DP_SOFT_EXIT ? DIAG_SOFT_EXIT : DIAG_FORCE_EXIT, 0, 0, 0, diag.exit_voter);
        break;
    }
out:
    raw_spin_unlock_irqrestore(&state_lock, flags);
    return 0;
}
NOKPROBE_SYMBOL(diag_pre);

static const char *diag_state(void)
{
    if (!diag.ready) return "unavailable";
    if (retiring) return "retired";
    if (!enabled) return "off";
    if (diag.exit && !diag.charging) return "exited";
    if (!bound_chip) return "waiting_pps";
    if (!diag.support && !diag.cp_ibus && (diag.queries || diag.charging)) return "data_unavailable";
    if (diag.session_used && diag.charging) return "used";
    return "armed";
}
static int get_diagnostics(char *buf, const struct kernel_param *param)
{
    unsigned long flags;
    int n;
    char raw[16];
    (void)param;
    raw_spin_lock_irqsave(&state_lock, flags);
    if (diag.raw_valid) snprintf(raw, sizeof(raw), "%u", diag.raw);
    else snprintf(raw, sizeof(raw), "null");
    n = snprintf(buf, PAGE_SIZE,
        "{\"api\":1,\"ready\":%s,\"error\":%d,\"generation_ms\":%llu,\"seq\":%llu,\"overwritten\":%llu,"
        "\"state\":\"%s\",\"charging\":%s,\"support\":%d,\"cp_ibus\":%d,"
        "\"queries\":%llu,\"successes\":%llu,\"failures\":%llu,\"uses\":%llu,\"resets\":%llu,"
        "\"query_ms\":%llu,\"rc\":%d,\"raw\":%s,\"used_ms\":%llu,\"exit\":\"%s\","
        "\"exit_voter\":\"%s\",\"disable_vote\":\"%s\",\"not_allow_vote\":\"%s\"}\n",
        diag.ready ? "true" : "false", diag.error, diag.generation_ms, diag.seq,
        diag.seq > DIAG_EVENTS ? diag.seq - DIAG_EVENTS : 0, diag_state(),
        diag.charging ? "true" : "false", diag.support, diag.cp_ibus,
        diag.queries, diag.successes, diag.failures, diag.uses, diag.resets,
        diag.query_ms, diag.rc, raw, diag.used_ms, diag.exit == 1 ? "soft" : diag.exit == 2 ? "force" : "none",
        diag.exit_voter, diag.disable_vote, diag.not_allow_vote);
    raw_spin_unlock_irqrestore(&state_lock, flags);
    return n;
}
static int get_events(char *buf, const struct kernel_param *param)
{
    unsigned long flags;
    u64 first, i;
    int n;
    (void)param;
    raw_spin_lock_irqsave(&state_lock, flags);
    first = diag.seq > DIAG_EVENTS ? diag.seq - DIAG_EVENTS + 1 : 1;
    n = snprintf(buf, PAGE_SIZE,
        "{\"api\":1,\"generation_ms\":%llu,\"first_seq\":%llu,\"next_seq\":%llu,\"overwritten\":%llu,\"events\":[",
        diag.generation_ms, first, diag.seq + 1, first - 1);
    /* 16 compact tuples, ASCII voter <=31 bytes: bounded well below 4 KiB,
     * including worst-case decimal widths. Reading never consumes the ring. */
    for (i = first; i <= diag.seq; i++) {
        const struct diag_event *e = &diag.events[(i - 1) % DIAG_EVENTS];
        n += snprintf(buf + n, PAGE_SIZE - n, "%s[%llu,%llu,%u,%u,%d,%lld,%d,%d,\"%s\"]",
            i == first ? "" : ",", e->seq, e->ms, e->kind, e->phase, e->rc,
            e->raw, e->support, e->current_ma, e->voter);
    }
    n += snprintf(buf + n, PAGE_SIZE - n, "]}\n");
    raw_spin_unlock_irqrestore(&state_lock, flags);
    return n;
}
static const struct kernel_param_ops diagnostics_ops = { .get = get_diagnostics };
static const struct kernel_param_ops events_ops = { .get = get_events };
module_param_cb(diagnostics, &diagnostics_ops, NULL, 0400);
module_param_cb(events, &events_ops, NULL, 0400);

static void diag_cleanup(void)
{
    while (diag_registered) unregister_kprobe(&diag_probes[--diag_registered]);
}
static void diag_init(void)
{
    unsigned i, j;
    diag.generation_ms = diag_now();
    for (i = 0; i < ARRAY_SIZE(diag_hooks); i++) {
        const struct diag_hook *site = &diag_hooks[i];
        struct kprobe lookup = { .symbol_name = site->symbol };
        const u8 *base;
        int rc = register_kprobe(&lookup);
        if (rc) { diag.error = rc; goto fail; }
        base = (const u8 *)lookup.addr;
        if (READ_ONCE(*(const u32 *)(base + site->offset)) != site->instruction) rc = -ENOEXEC;
        for (j = 0; j < ARRAY_SIZE(site->anchors); j++)
            if (READ_ONCE(*(const u32 *)(base + site->anchors[j].offset)) != site->anchors[j].word) rc = -ENOEXEC;
        unregister_kprobe(&lookup);
        if (rc) { diag.error = rc; goto fail; }
        diag_probes[i].symbol_name = site->symbol;
        diag_probes[i].offset = site->offset;
        diag_probes[i].pre_handler = diag_pre;
        rc = register_kprobe(&diag_probes[i]);
        if (rc) { diag.error = rc; goto fail; }
        diag_registered++;
    }
    diag.ready = true;
    return;
fail:
    diag_cleanup(); /* Optional diagnostics must not disable the existing assist. */
}
