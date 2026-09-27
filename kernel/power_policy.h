/* SPDX-License-Identifier: GPL-2.0-only */
/* Pure policy, also exercised by the native host tests. Units: W, mV, mA. */
#ifndef CG_POWER_POLICY_H
#define CG_POWER_POLICY_H
enum cg_protocol { CG_NONE, CG_SVOOC, CG_UFCS, CG_PPS, CG_PD };
struct cg_filter { int applied, candidate, confirmations; };
static int cg_limit(int protocol, unsigned watts, int mv)
{
    static const int svooc[] = {1500,2000,2500,3000,3500,4000,4500,5000,
        5500,6000,6300,6500,7000,7500,8000,8500,9000,9500};
    unsigned raw, i;
    int out = 0, step;
    if (watts < 20 || watts > 100 || mv < 4000 || mv > 22000)
        return 0;
    raw = watts * 1000000U / (unsigned)mv;
    if (protocol == CG_SVOOC) {
        /* Pinned 7-bit table type 2; exact entries prevent OEM rounding up. */
        for (i = 0; i < sizeof(svooc)/sizeof(svooc[0]); i++)
            if ((unsigned)svooc[i] <= raw) out = svooc[i];
        return out;
    }
    /* PPS PDO resolution 50mA. UFCS uses 20mA, divisible by both 10/20mA protocol steps.
     * For PD submit conservative 50mA increments to the OEM ICL API. */
    step = protocol == CG_UFCS ? 20 : 50;
    if (protocol < CG_UFCS || protocol > CG_PD) return 0;
    out = (int)(raw / step) * step;
    return out < 500 ? 0 : out;
}
static int cg_filter_next(struct cg_filter *f, int next, int sample)
{
    if (!next) return 0;
    if (!f->applied || next < f->applied) {
        f->candidate = f->confirmations = 0;
        return next;
    }
    if (next == f->applied) {
        f->candidate = f->confirmations = 0;
        return next;
    }
    if (!sample) return f->applied;
    if (f->candidate != next) {
        f->candidate = next; f->confirmations = 1;
        return f->applied;
    }
    if (++f->confirmations >= 2) {
        f->candidate = f->confirmations = 0;
        return next;
    }
    return f->applied;
}
#endif
