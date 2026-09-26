/* Original minimal adapter for KernelSU's documented exec bridge; no remote JS. */
(function(root){'use strict';
  const executable='/data/adb/modules/charge_guard/bin/cg';
  const verbs=new Set(['open-repository','open-author','ui-initial','ui-status','device-info','logs','stop','status','thermal-nodes','config','pause','recover','start','arm','export','diagnose','version']);
  let sequence=0;
  function available(){return !!(root.ksu && typeof root.ksu.exec==='function');}
  function command(verb,payload){
    if(verb==='configure-hex'){
      if(typeof payload!=='string'||payload.length>8192||payload.length%2||!/^[0-9a-f]+$/.test(payload))throw Error('非法配置编码');
      return executable+' configure-hex '+payload;
    }
    if(!verbs.has(verb)||payload!==undefined)throw Error('非法后端命令');
    return executable+' '+verb;
  }
  function call(verb,payload,timeout=20000){
    let cmd;try{cmd=command(verb,payload);}catch(e){return Promise.reject(e);}
    if(!available())return Promise.reject(Error('宿主未提供 KernelSU WebUI API；请使用 Root CLI'));
    return new Promise((resolve,reject)=>{
      const cb='chargeGuardCallback'+(++sequence);let done=false;
      const finish=(err,value)=>{if(done)return;done=true;clearTimeout(timer);delete root[cb];if(err)reject(err);else resolve(value);};
      const timer=setTimeout(()=>finish(Error('命令超时；执行结果未知，请刷新，勿直接假定成功')),timeout);
      root[cb]=(code,stdout,stderr)=>{
        if(Number(code)!==0)return finish(Error(String(stderr||'后端执行失败').slice(0,1200)));
        try{finish(null,JSON.parse(String(stdout)));}catch(e){finish(Error('后端返回格式错误'));}
      };
      try{root.ksu.exec(cmd,'{}',cb);}catch(e){finish(e);}
    });
  }
  function hex(value){return Array.from(new TextEncoder().encode(JSON.stringify(value)),b=>b.toString(16).padStart(2,'0')).join('');}
  root.CG={available,call,command,hex};
})(globalThis);
