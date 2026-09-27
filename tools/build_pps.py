#!/usr/bin/env python3
"""Build the explicitly pinned PJZ110 LKM from device IKHEADERS and symbol CRCs.

No forced vermagic, CRC bypass, proprietary driver binary, or image modification.
Uses the NDK LLVM tools already required by the normal project build.
"""
import hashlib,json,pathlib,struct,subprocess,tarfile

ROOT=pathlib.Path(__file__).resolve().parents[1]
KERNEL=ROOT/'kernel'

def sections(path):
    data=path.read_bytes()
    assert data[:6]==b'\x7fELF\x02\x01' and struct.unpack_from('<H',data,18)[0]==183
    offset=struct.unpack_from('<Q',data,40)[0]
    size,count,names=struct.unpack_from('<HHH',data,58)
    sh=[struct.unpack_from('<IIQQQQIIQQ',data,offset+i*size) for i in range(count)]
    strings=data[sh[names][4]:sh[names][4]+sh[names][5]]
    result={}
    for s in sh:
        name=strings[s[0]:].split(b'\0')[0].decode()
        result[name]=(s,data[s[4]:s[4]+s[5]] if s[1]!=8 else b'')
    return result,sh,data

def undefined(path):
    sec,sh,data=sections(path);s,raw=sec['.symtab']
    strings=data[sh[s[6]][4]:sh[s[6]][4]+sh[s[6]][5]]
    result=set()
    for off in range(0,len(raw),24):
        name,info,_,index,_,_=struct.unpack_from('<IBBHQQ',raw,off)
        if name and not index and info>>4 in (1,2):
            result.add(strings[name:].split(b'\0')[0].decode())
    return result

def build(llvm,output,name="charge_guard_pps",profile_name="pjz110-target.json",symbols_name="pjz110-symbols.json"):
    llvm=pathlib.Path(llvm);output=pathlib.Path(output)
    win=(llvm/'clang.exe').exists();ext='.exe' if win else ''
    compiler=llvm/('clang'+ext);linker=llvm/('ld.lld'+ext)
    work=ROOT/('target/'+name+'-lkm');headers=work/'headers';work.mkdir(parents=True,exist_ok=True)
    archive=KERNEL/'pjz110-headers.tar.xz'
    profile=json.loads((KERNEL/profile_name).read_text())
    assert hashlib.sha256(archive.read_bytes()).hexdigest()==profile['headers_sha256']
    if not (headers/'include/generated/utsrelease.h').exists():
        headers.mkdir(exist_ok=True)
        with tarfile.open(archive) as tar:
            tar.extractall(headers,filter='data')
    release=(headers/'include/generated/utsrelease.h').read_text()
    assert '"'+profile['kernel_release']+'"' in release
    includes=['arch/arm64/include','arch/arm64/include/generated','include',
              'arch/arm64/include/uapi','arch/arm64/include/generated/uapi','include/uapi','include/generated/uapi']
    flags=['--target=aarch64-linux-gnu','-std=gnu11','-O2','-ffreestanding','-fno-builtin',
           '-fno-pic','-fno-pie','-fno-stack-protector','-fno-asynchronous-unwind-tables',
           '-fno-strict-aliasing','-fno-common','-mgeneral-regs-only','-mno-outline-atomics',
           '-mbranch-protection=pac-ret','-fsanitize=kcfi','-D__KERNEL__','-DMODULE',
           '-DKBUILD_MODNAME="'+name+'"','-DKBUILD_BASENAME="'+name+'"',
           '-include',str(headers/'include/linux/kconfig.h'),'-Wall','-Wextra',
           '-Wno-unused-parameter','-Wno-sign-compare','-Wno-pointer-sign',
           '-ffile-prefix-map='+str(ROOT)+'=/src/charge-guard']
    for inc in includes:flags+=['-I',str(headers/inc)]
    flags+=['-I',str(work)]
    if name=='charge_guard_power':
        from build_power import write_anchors
        write_anchors(profile,work/'power_anchors.h')
    obj=work/(name+'.o')
    subprocess.run([compiler,*flags,'-c',KERNEL/(name+'.c'),'-o',obj],check=True)
    symbols=json.loads((KERNEL/symbols_name).read_text())
    imports=(undefined(obj)-{'__this_module'})|{'module_layout'}
    missing=imports-symbols.keys()
    if missing:raise ValueError('No verified kernel CRC for: '+', '.join(sorted(missing)))
    generated='''#include <linux/module.h>
#define INCLUDE_VERMAGIC
#include <linux/vermagic.h>
MODULE_INFO(vermagic, VERMAGIC_STRING);
MODULE_INFO(name, KBUILD_MODNAME);
MODULE_INFO(depends, "");
extern int init_module(void);
extern void cleanup_module(void);
__visible struct module __this_module __section(".gnu.linkonce.this_module") = {
 .name = KBUILD_MODNAME, .init = init_module, .exit = cleanup_module,
 .arch = MODULE_ARCH_INIT,
};
static const struct modversion_info cg_versions[] __used __section("__versions") = {
'''
    generated+=''.join(f' {{ 0x{symbols[n]:08x}, "{n}" }},\n' for n in sorted(imports))+'};\n'
    mod=work/'charge_guard_pps.mod.c';mod.write_text(generated,encoding='utf-8',newline='\n')
    modobj=work/'charge_guard_pps.mod.o'
    subprocess.run([compiler,*flags,'-c',mod,'-o',modobj],check=True)
    output.parent.mkdir(parents=True,exist_ok=True)
    # ARM64 module loader reserves these sections for out-of-range branch PLTs.
    lds=work/'module.lds'
    lds.write_text('SECTIONS { .plt 0 : { BYTE(0) } .init.plt 0 : { BYTE(0) } .text.ftrace_trampoline 0 : { BYTE(0) } }\n',encoding='ascii')
    subprocess.run([linker,'-r','--build-id=sha1','-T',lds,obj,modobj,'-o',output],check=True)
    sec,_,_=sections(output)
    assert profile['kernel_release'].encode() in sec['.modinfo'][1]
    assert all(n in sec for n in ('.plt','.init.plt','.text.ftrace_trampoline'))
    assert not (undefined(output)-imports), 'unversioned imports introduced by module metadata'
    raw=output.read_bytes()
    assert profile['api'].encode() in raw
    consumer=(ROOT/('src/pps.rs' if name=='charge_guard_pps' else 'src/power.rs')).read_text(encoding='utf-8')
    assert profile['api'] in consumer
    if name=='charge_guard_pps':assert all(profile[k] in consumer for k in ('kernel_release','driver_sha256'))
    assert b'__kcfi_typeid_' in raw, 'KCFI metadata required'
    assert str(ROOT).encode() not in raw
    print(name+': pinned ARM64 / KCFI / original symbol CRCs / disabled by default:',output)
    return {'sha256':hashlib.sha256(raw).hexdigest(),'imports':sorted(imports),'kernel_release':profile['kernel_release']}
