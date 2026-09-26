#!/usr/bin/env python3
"""Build/check the project from locked inputs; all generated files live under target."""
import argparse,html,json,os,pathlib,re,shutil,stat,subprocess,sys,tomllib,zipfile
ROOT=pathlib.Path(__file__).resolve().parents[1]
RUNTIME=('module.prop','customize.sh','service.sh','post-fs-data.sh','uninstall.sh','action.sh','skip_mount',
         'webroot/index.html','webroot/style.css','webroot/theme.js','webroot/bridge.js','webroot/app.js',
         'META-INF/com/google/android/update-binary','META-INF/com/google/android/updater-script')
SOURCE=('Cargo.toml','Cargo.lock','build.rs','rust-toolchain.toml','.gitattributes','.gitignore')
BUILD_SUPPORT=('tools/build.py','tools/ci.py','tools/cooling_events.c',
               '.github/workflows/beta.yml','.github/workflows/release.yml',
               '.github/actions/build/action.yml')
STAMP=(2026,1,1,0,0,0)
def run(args,**kwargs):
    return subprocess.run([str(a) for a in args],check=True,cwd=ROOT,**kwargs)
def metadata():
    assert not (ROOT/'module/system.prop').exists(), 'system.prop is not part of this module'
    m=dict(line.split('=',1) for line in (ROOT/'module/module.prop').read_text(encoding='utf-8').splitlines() if '=' in line)
    assert re.fullmatch('[A-Za-z][A-Za-z0-9_]*',m['id'])
    assert m['version']==tomllib.loads((ROOT/'Cargo.toml').read_text(encoding='utf-8'))['package']['version']
    assert int(m['versionCode'])>0
    return m

def archive(path,items):
    path.parent.mkdir(parents=True,exist_ok=True)
    with zipfile.ZipFile(path,'w',zipfile.ZIP_DEFLATED,compresslevel=9) as z:
        for name,data,executable in sorted(items):
            assert not pathlib.PurePosixPath(name).is_absolute() and '..' not in pathlib.PurePosixPath(name).parts
            assert not any(x in name.lower() for x in ['license','licence','.md','.sha256','__pycache__','node_modules'])
            info=zipfile.ZipInfo(name,STAMP);info.create_system=3
            info.external_attr=(stat.S_IFREG|(0o755 if executable else 0o644))<<16
            z.writestr(info,data,compress_type=zipfile.ZIP_DEFLATED,compresslevel=9)
    with zipfile.ZipFile(path) as z:
        assert z.testzip() is None and len(z.namelist())==len(items)
        assert len(set(z.namelist()))==len(items)
    print(path.name,path.stat().st_size,'bytes')
def normalized(p):
    return p.read_text(encoding='utf-8-sig').replace('\r\n','\n').replace('\r','\n').encode('utf-8')

def source_files():
    files=[ROOT/n for n in (*SOURCE,*BUILD_SUPPORT)]+[ROOT/'module'/n for n in RUNTIME]
    for folder,suffix in [('src','.rs'),('observer/src','.java')]:
        files.extend(p for p in (ROOT/folder).rglob('*') if p.is_file() and p.suffix==suffix)
    return sorted(files)

def audit(a,m):
    allowed={p.relative_to(ROOT).as_posix() for p in source_files()}
    allowed.update(('README.md','LICENSE'))
    tracked=run(['git','ls-files','-z'],capture_output=True).stdout.decode('utf-8').split('\0')
    forbidden=[p for p in tracked if p and (p not in allowed or 'local_docs' in p.lower().split('/'))]
    if forbidden:raise SystemExit('Files outside build allowlist: '+', '.join(forbidden))
    print('Git index contains only build inputs, README and LICENSE; local_docs is excluded.')

def channel_metadata(m,channel,beta_number=None):
    match=re.fullmatch(r'(\d+\.\d+\.\d+)(?:-(beta|release)([1-9]\d*)?)?',m['version'])
    if not match:raise SystemExit('--channel requires a base, beta or release version such as 1.0.0')
    if match[2]=='release' and match[3]:raise SystemExit('release must not have a numeric suffix')
    release_code=int(m['versionCode'])
    if match[2]=='beta':release_code+=10000-int(match[3]) if match[3] else 1
    number=beta_number if beta_number is not None else int(match[3] or 1)
    if channel=='beta' and not 1<=number<=9999:raise SystemExit('beta number must be between 1 and 9999')
    version=match[1]+'-'+channel+(str(number) if channel=='beta' else '')
    code=release_code-10000+number if channel=='beta' else release_code
    if code<=0:raise SystemExit('versionCode must reserve 10000 values below release for beta builds')
    return {**m,'version':version,'versionCode':str(code)}

def channel_build(a,m):
    # Materialize channel metadata under target. Never rewrite the checkout.
    generated=channel_metadata(m,a.channel,a.beta_number);version=generated['version']
    work=ROOT/'target/channel-build'/a.channel
    if work.exists():shutil.rmtree(work)
    for p in source_files():
        dest=work/p.relative_to(ROOT);dest.parent.mkdir(parents=True,exist_ok=True)
        dest.write_bytes(normalized(p))
    for name in ('Cargo.toml','Cargo.lock','module/module.prop'):
        p=work/name;s=p.read_text(encoding='utf-8')
        if name=='module/module.prop':
            s=s.replace('version='+m['version']+'\n','version='+version+'\n')
            s=re.sub(r'(?m)^versionCode=\d+$','versionCode='+generated['versionCode'],s)
        else:
            # Limit replacement to this package, never a dependency's version.
            pattern=r'(name\s*=\s*"charge-guard"\s*\nversion\s*=\s*")'+re.escape(m['version'])+r'(")'
            s,n=re.subn(pattern,lambda match:match[1]+version+match[2],s)
            assert n==1,(name,n)
        p.write_text(s,encoding='utf-8',newline='\n')
    output=pathlib.Path(a.output).resolve() if a.output else ROOT/'target/dist'
    command=[sys.executable,work/'tools/build.py','build','--output',output]
    for flag in ('sdk','ndk','jdk'):
        if getattr(a,flag):command+=['--'+flag,pathlib.Path(getattr(a,flag)).resolve()]
    if a.offline:command.append('--offline')
    if a.installation_only:command.append('--installation-only')
    run(command)
def package(stage,out,m,installation_only=False):
    names=(*RUNTIME,'bin/cg','bin/cg-camera.jar','bin/cg-cooling-events')
    assert not (stage/'system.prop').exists()
    filename=f"{m['id']}_v{m['version']}.zip" if installation_only else f"{m['id']}_{m['version']}_magisk.zip"
    archive(out/filename,[(n,(stage/n).read_bytes(),n.endswith('.sh') or n in ('bin/cg','bin/cg-cooling-events') or n.endswith('/update-binary')) for n in names])
    if installation_only:return
    files=source_files()
    archive(out/f"{m['id']}_{m['version']}_source.zip",[("charge-guard/"+p.relative_to(ROOT).as_posix(),normalized(p),p.suffix=='.sh' or p.name=='update-binary') for p in files])
def path_variants(path):
    raw=pathlib.Path(path)
    if not raw.is_absolute():raw=ROOT/raw
    return {str(raw),raw.as_posix(),str(raw.resolve()),raw.resolve().as_posix()}

def source_path_mappings(env,sdk,ndk,jdk):
    cargo_home=pathlib.Path(env.get('CARGO_HOME') or pathlib.Path.home()/'.cargo')
    sysroot=pathlib.Path(run(['rustc','--print','sysroot'],capture_output=True,text=True).stdout.strip())
    mappings=[(pathlib.Path.home(),'/build-home'),(cargo_home,'/cargo'),(sysroot,'/rust-toolchain'),
              (sdk,'/android-sdk'),(ndk,'/android-ndk'),(jdk,'/java-toolchain'),(ROOT,'/src/charge-guard')]
    rules={}
    for source,destination in mappings:
        for prefix in path_variants(source):
            if len(pathlib.Path(prefix).parts)>1:rules[prefix]=destination
    # rustc uses the last matching rule. Prefer the most specific prefix, including
    # a custom CARGO_HOME nested inside the checkout, and both Windows separators.
    flags=[f'--remap-path-prefix={src}={dst}' for src,dst in sorted(rules.items(),key=lambda item:(len(item[0]),item[0]))]
    return flags,tuple(rules)

def verify_no_build_paths(data,prefixes):
    lower=data.lower()
    for prefix in prefixes:
        for encoding in ('utf-8','utf-16le'):
            if prefix.encode(encoding).lower() in lower:raise ValueError('Build-machine path remains in binary: '+prefix)
    if re.search(rb'[A-Za-z]:\\|(?<![A-Za-z0-9_])[A-Za-z]:/[A-Za-z0-9_.-]+/',data):
        raise ValueError('Windows absolute path remains in binary')
    if re.search(rb'/(?:home|Users)/[^/\x00]+/|/mnt/[a-z]/',data):
        raise ValueError('Host home or workspace path remains in binary')

def verify_elf(binary,readelf,local_prefixes):
    data=binary.read_bytes();verify_no_build_paths(data,local_prefixes);assert data[:6]==b'\x7fELF\x02\x01'
    assert int.from_bytes(data[18:20],'little')==183 and int.from_bytes(data[16:18],'little')==3
    def output(flag):return run([readelf,flag,binary],capture_output=True,text=True).stdout
    headers=output('-lW');dynamic=output('-dW');versions=output('-VW')
    assert '/system/bin/linker64' in headers and 'GLIBC_' not in versions
    assert 'libc.so]' in dynamic and 'libc.so.6' not in dynamic and 'BIND_NOW' in dynamic and 'GNU_RELRO' in headers
    for line in headers.splitlines():
        if line.strip().startswith('LOAD'):assert int(line.split()[-1],16)>=16384
        if line.strip().startswith('GNU_STACK'):assert all('E' not in field for field in line.split()[6:-1])
    assert b'CG_FIXTURE_ROOT' not in data
    assert all(token not in data for token in [b'/system/bin/setprop',b'resetprop',b'ctl.start',b'ctl.stop',b'ctl.restart']), 'Forbidden property-based service control in production binary'
    print('ELF: ARM64 / Bionic / API 35 / 16 KiB / production / no build-machine paths')
def android_tools(a):
    env=os.environ.copy();sdk=pathlib.Path(a.sdk or env.get('ANDROID_SDK_ROOT') or env.get('ANDROID_HOME') or '').resolve()
    if not (sdk/'platforms/android-35/android.jar').is_file():raise SystemExit('Specify --sdk with the Android 35 platform installed')
    ndk=pathlib.Path(a.ndk or env.get('ANDROID_NDK_HOME') or sdk/'ndk/27.3.13750724').resolve()
    assert 'Pkg.Revision = 27.3.13750724' in (ndk/'source.properties').read_text()
    llvm=ndk/('toolchains/llvm/prebuilt/windows-x86_64/bin' if os.name=='nt' else 'toolchains/llvm/prebuilt/linux-x86_64/bin')
    return env,sdk,ndk,llvm
def cooling_listener(env,llvm,binary):
    compiler=llvm/('clang.exe' if os.name=='nt' else 'clang')
    run([compiler,'--target=aarch64-linux-android35','-std=c11','-O2','-Wall','-Wextra','-fPIE','-pie','-Wl,-z,max-page-size=16384','-Wl,-z,common-page-size=16384','-Wl,-z,relro,-z,now','-s',
         '-ffile-prefix-map='+str(ROOT)+'=/src/charge-guard',ROOT/'tools/cooling_events.c','-o',binary],env=env)
    verify_elf(binary,llvm/('llvm-readelf.exe' if os.name=='nt' else 'llvm-readelf'),path_variants(ROOT)|path_variants(pathlib.Path.home()))
    print('Cooling event transport:',binary)
def build(a,m):
    env,sdk,ndk,llvm=android_tools(a)
    assert run(['rustc','--version'],capture_output=True,text=True).stdout.startswith('rustc 1.85.1 ')
    jdk=pathlib.Path(a.jdk or env.get('JAVA_HOME') or pathlib.Path(shutil.which('javac') or '').parent.parent).resolve()
    exe='.exe' if os.name=='nt' else '';java=jdk/'bin'/('java'+exe);javac=jdk/'bin'/('javac'+exe)
    assert java.is_file() and javac.is_file(),'Specify --jdk'
    work=ROOT/'target/release-build';stage=work/'module';classes=work/'classes';dex=work/'dex'
    for folder in [stage,classes,dex]:
        if folder.exists():shutil.rmtree(folder)
        folder.mkdir(parents=True)
    for name in RUNTIME:
        p=stage/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(normalized(ROOT/'module'/name))
    index=stage/'webroot/index.html';s=index.read_text(encoding='utf-8')
    s=re.sub(r'<title>.*?</title>','<title>'+html.escape(m['name'])+'</title>',s)
    s=re.sub(r'(<h1 id="page-title">).*?(</h1>)',lambda x:x[1]+html.escape(m['name'])+x[2],s)
    s=re.sub(r'(<strong id="module-author"[^>]*>).*?(</strong>)',lambda x:x[1]+html.escape(m['author'])+x[2],s)
    s=re.sub(r'<p class="version">.*?</p>','<p class="version">'+html.escape(m['version'])+'</p>',s)
    index.write_text(s,encoding='utf-8',newline='\n')
    android=sdk/'platforms/android-35/android.jar'
    run([javac,'-encoding','UTF-8','-source','8','-target','8','-Xlint:-options','-classpath',android,'-d',classes,ROOT/'observer/src/com/chargeguard/CameraEvents.java'])
    run([java,'-cp',sdk/'build-tools/36.0.0/lib/d8.jar','com.android.tools.r8.D8','--min-api','35','--lib',android,'--output',dex,*sorted(classes.rglob('*.class'))])
    (stage/'bin').mkdir()
    with zipfile.ZipFile(stage/'bin/cg-camera.jar','w',zipfile.ZIP_DEFLATED) as z:
        info=zipfile.ZipInfo('classes.dex',STAMP);info.compress_type=zipfile.ZIP_DEFLATED;z.writestr(info,(dex/'classes.dex').read_bytes())
    env['CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER']=str(llvm/('aarch64-linux-android35-clang.cmd' if os.name=='nt' else 'aarch64-linux-android35-clang'))
    path_flags,local_prefixes=source_path_mappings(env,sdk,ndk,jdk)
    env['CARGO_ENCODED_RUSTFLAGS']='\x1f'.join(path_flags+['-C','link-arg=-Wl,-z,max-page-size=16384','-C','link-arg=-Wl,-z,common-page-size=16384'])
    args=['cargo','build','--release','--target','aarch64-linux-android','--locked','--target-dir',work/'cargo']
    if a.offline:args.append('--offline')
    run(args,env=env)
    binary=work/'cargo/aarch64-linux-android/release/charge-guard';verify_elf(binary,llvm/('llvm-readelf'+exe),local_prefixes);shutil.copyfile(binary,stage/'bin/cg')
    cooling_listener(env,llvm,stage/'bin/cg-cooling-events')
    package(stage,pathlib.Path(a.output).resolve() if a.output else ROOT/'target/dist',m,a.installation_only)
def linux_path(p):
    p=pathlib.Path(p).resolve();return '/mnt/'+p.drive[0].lower()+p.as_posix().split(':',1)[1]
def check(a,m):
    env=os.environ.copy();target=ROOT/'target/check'
    args=['cargo','test','--locked','--features','fixtures','--target-dir',target,'--no-run','--message-format=json']
    if os.name=='nt':
        triple='x86_64-unknown-linux-musl';sysroot=pathlib.Path(run(['rustc','--print','sysroot'],capture_output=True,text=True).stdout.strip())
        env['CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER']=str(sysroot/'lib/rustlib/x86_64-pc-windows-msvc/bin/rust-lld.exe')
        env['CARGO_ENCODED_RUSTFLAGS']='-C\x1flinker-flavor=ld.lld';args+=['--target',triple]
    else:triple=''
    if a.offline:args.append('--offline')
    output=run(args,env=env,capture_output=True,text=True)
    print(output.stderr[-2500:])
    artifacts=[json.loads(line) for line in output.stdout.splitlines() if line.startswith('{')]
    def execute(cmd):
        if os.name=='nt':run(['wsl','-d',a.wsl,'-u','root','--',*cmd])
        else:run(cmd)
    for item in artifacts:
        if item.get('reason')=='compiler-artifact' and item.get('executable') and item['profile']['test']:
            executable=item['executable'];execute([linux_path(executable) if os.name=='nt' else executable,'--test-threads=1'])
    build_args=['cargo','build','--locked','--features','fixtures','--target-dir',target]
    if triple:build_args+=['--target',triple]
    if a.offline:build_args+=['--offline']
    run(build_args,env=env)
    binary=target/triple/'debug/charge-guard'
    native=target/'cg-cooling-events'
    if os.name=='nt':
        native_env,_,_,llvm=android_tools(a)
        cc=[llvm/'clang.exe','--target=x86_64-linux-android35','-static']
    else:
        native_env=env;cc=[env.get('CC','cc')]
    run([*cc,'-std=c11','-O2','-Wall','-Wextra',ROOT/'tools/cooling_events.c','-o',native],env=native_env)
    for name in ['lifecycle.py','installer.py','cooling_events.py']:
        script=ROOT/'tests'/name
        cmd=['python3',linux_path(script) if os.name=='nt' else str(script)]
        if name=='lifecycle.py':cmd+=['--binary',linux_path(binary) if os.name=='nt' else str(binary)]
        if name=='cooling_events.py':cmd+=['--binary',linux_path(native) if os.name=='nt' else str(native)]
        execute(cmd)
    for p in (ROOT/'module/webroot').glob('*.js'):run(['node','--check',p])
    print('Core, lifecycle, installer and JavaScript syntax checks passed; no UI automation run.')
if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('command',choices=['build','check','audit'],nargs='?',default='build')
    for flag in ['sdk','ndk','jdk','output']:parser.add_argument('--'+flag)
    parser.add_argument('--channel',choices=['beta','release'])
    parser.add_argument('--beta-number',type=int)
    parser.add_argument('--installation-only',action='store_true')
    parser.add_argument('--offline',action='store_true');parser.add_argument('--wsl',default='Debian')
    a=parser.parse_args();m=metadata()
    if a.channel and a.command!='build':parser.error('--channel is only valid for build')
    if a.beta_number is not None and a.channel!='beta':parser.error('--beta-number requires --channel beta')
    if a.installation_only and a.command!='build':parser.error('--installation-only is only valid for build')
    try:(channel_build if a.channel else {'check':check,'build':build,'audit':audit}[a.command])(a,m)
    except subprocess.CalledProcessError as e:
        if e.stdout:print(e.stdout[-5000:])
        if e.stderr:print(e.stderr[-5000:],file=sys.stderr)
        raise SystemExit(e.returncode)
