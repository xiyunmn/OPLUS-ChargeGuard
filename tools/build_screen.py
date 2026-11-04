#!/usr/bin/env python3
"""Build the shared charging-screen helper with the original kernel ABI."""
from build_pps import build as build_lkm

NAMES=('oplus_mms_get_by_name','oplus_mms_get_item_data','oplus_mms_subscribe',
       'oplus_mms_unsubscribe','oplus_mms_put','oplus_mms_alloc_msg',
       'oplus_mms_publish_msg_sync','oplus_comm_update_led_on')

def write_anchors(profile,path):
    text='static const struct { const char *symbol; unsigned kcfi, words[3]; } cg_anchors[] = {\n'
    for name in NAMES:
        a=profile['anchors'][name]
        text+=' {"oplus_chg_v2:%s",0x%08x,{%s}},\n'%(name,a['kcfi'],','.join('0x%08x'%x for x in a['entry']))
    text+='};\n#define CG_LED_PROBE_OFFSET 0x%x\n'%profile['led_hook']['offset']
    text+='static const struct { unsigned offset, word; } cg_led_anchors[] = {\n'
    for offset,word in profile['led_hook']['words'].items():
        text+=' {0x%x,0x%08x},\n'%(int(offset),word)
    path.write_text(text+'};\n',encoding='ascii')

def build(llvm,output):
    return build_lkm(llvm,output,'charge_guard_screen','screen-target.json','screen-symbols.json')
