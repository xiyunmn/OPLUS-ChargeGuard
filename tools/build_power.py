#!/usr/bin/env python3
"""Build the separate opt-in power voter. ABI evidence is a required build input."""
from build_pps import build as build_lkm

NAMES=('vote','find_votable','get_client_vote','is_client_vote_enabled',
       'get_effective_result','oplus_mms_get_by_name','oplus_mms_get_item_data',
       'oplus_mms_subscribe','oplus_mms_unsubscribe','oplus_mms_put','oplus_wired_get_vbus')

def write_anchors(profile, path):
    text='static const struct { const char *symbol; unsigned kcfi, words[3]; } cg_anchors[] = {\n'
    for name in NAMES:
        a=profile['anchors'][name]
        text+=' {"oplus_chg_v2:%s",0x%08x,{%s}},\n'%(name,a['kcfi'],','.join('0x%08x'%x for x in a['entry']))
    path.write_text(text+'};\n',encoding='ascii')

def build(llvm,output):
    return build_lkm(llvm,output,'charge_guard_power','pjz110-power-target.json','pjz110-power-symbols.json')
