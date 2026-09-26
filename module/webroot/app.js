(function(){
'use strict';
const $=id=>document.getElementById(id);
const pages={overview:document.title,global:'全局温控',charge:'充电预设',preferences:'设置'};
const tempKeys=['batt_temp_mc','cpu_temp_mc','gpu_temp_mc','ddr_temp_mc','charge_batt_temp_mc','charge_cpu_temp_mc','charge_gpu_temp_mc','charge_ddr_temp_mc'];
const counts=new Intl.NumberFormat('en-US');
const stateNames={applied:'已应用',unsupported:'不支持',unavailable:'不可用',retrying:'待重试',restored:'已恢复',restore_pending:'恢复中'};
const profileNames={charging:'充电独立预设',global:'全局温控预设',idle:'未启用预设'};
const chargeNames={Charging:'充电中',Full:'已充满',Discharging:'使用电池','Not charging':'未充电'};
const modeNames={event:'纯事件',event_check:'事件＋周期核验',loop:'周期写入',dormant:'目标不存在'};
let requestEpoch=0,pollGeneration=0,pollTimer=null,pollingActive=false,viewOpen=true;
let config=null,dirty=false,busy=false,refreshing=false,loadingLogs=false,loadingNodes=false;
let currentPage='overview',lastStatus=null,nodeIdentity='',nodeFilter='all',logFilter='all',backendFilter='all';
let diagnosticTab='logs',diagnosticExpanded=false,nodes=[],logEntries=[],logsLoaded=false;
const pageScroll=new Map(),logRecords=new Map(),backendRecords=new Map();

if(CG.available()){
  const link=document.createElement('link');link.rel='stylesheet';link.href='/internal/insets.css';document.head.append(link);
}
function element(tag,text,cls){const e=document.createElement(tag);if(text!=null)e.textContent=text;if(cls)e.className=cls;return e;}
function setText(node,value){const next=String(value??'—');if(node.textContent!==next)node.textContent=next;}
function icon(name){
  const ns='http://www.w3.org/2000/svg',svg=document.createElementNS(ns,'svg'),use=document.createElementNS(ns,'use');
  svg.setAttribute('viewBox','0 0 24 24');svg.setAttribute('class','icon');svg.setAttribute('aria-hidden','true');use.setAttribute('href','#i-'+name);svg.append(use);return svg;
}
function number(v){if(v==null||v==='')return null;const n=Number(v);return Number.isFinite(n)?n:null;}
function count(v){const n=number(v);return n==null?'—':counts.format(n);}
function uptime(ms){const n=number(ms);if(n==null)return '—';const s=Math.floor(Math.max(0,n)/1000);return [Math.floor(s/3600),Math.floor(s/60)%60,s%60].map(v=>String(v).padStart(2,'0')).join(':');}
function requestCurrent(epoch,generation){return epoch===requestEpoch&&generation===pollGeneration&&!document.hidden&&viewOpen;}
function message(text,bad=false,kind='runtime'){
  const node=$('message');node.dataset.kind=kind;node.hidden=!text||(kind==='config'&&currentPage==='overview');setText(node,text||'');node.classList.toggle('error',bad);
}
function controls(){
  const unavailable=busy||!CG.available();
  for(const node of document.querySelectorAll('.backend'))node.disabled=unavailable;
  for(const node of document.querySelectorAll('#settings input:not([name=theme])'))node.disabled=unavailable||!config;
  for(const node of document.querySelectorAll('[name=charge_horae_mode]'))node.disabled=unavailable||!config||!$('charge_horae_enabled').checked;
  for(const button of document.querySelectorAll('[data-adjust]')){
    const slider=$(button.dataset.target),v=Number(slider.value),step=Number(button.dataset.adjust);
    button.disabled=unavailable||!config||(step<0?v<=Number(slider.min):v>=Number(slider.max));
  }
  $('savebar').hidden=!dirty||currentPage==='overview';
}
function labels(){
  for(const key of tempKeys){
    const input=$(key),v=Number(input.value);setText($(key+'_value'),v+'℃');input.setAttribute('aria-valuetext',v+'摄氏度');
    input.style.setProperty('--fill',100*(v-Number(input.min))/(Number(input.max)-Number(input.min))+'%');
  }
  const event=document.querySelector('[name=write_mode]:checked')?.value!=='loop';
  setText($('write-mode-hint'),event?'默认使用事件驱动':'按现有周期维护');
  const smart=document.querySelector('[name=charge_horae_mode]:checked')?.value==='smart';
  setText($('horae-mode-hint'),smart?'相机占用或未知时恢复，空闲稳定 3 秒后禁用。':'充电预设运行期间持续禁用 Horae。');
  setText($('global-flag'),$('global_enabled').checked?'开启':'关闭');$('global-flag').dataset.tone=$('global_enabled').checked?'good':'muted';
  setText($('theme-hint'),{light:'当前为浅色外观',dark:'当前为深色外观',system:'跟随系统外观'}[CGTheme.get()]);
}
function populate(next){
  config=next;
  for(const key of tempKeys)$(key).value=next[key]/1000;
  for(const key of ['global_enabled','horae_stop','charge_trigger','charge_horae_enabled','detailed_logging'])$(key).checked=!!next[key];
  for(const radio of document.querySelectorAll('[name=charge_horae_mode]'))radio.checked=radio.value===next.charge_horae_mode;
  for(const radio of document.querySelectorAll('[name=write_mode]'))radio.checked=radio.value===(next.write_mode||'event');
  dirty=false;labels();controls();
}
function page(key){
  if(!(key in pages)||key===currentPage)return;
  requestEpoch++;pageScroll.set(currentPage,$('content').scrollTop);currentPage=key;
  for(const name of Object.keys(pages)){
    $('page-'+name).hidden=name!==key;$('tab-'+name).setAttribute('aria-selected',String(name===key));$('tab-'+name).tabIndex=name===key?0:-1;
  }
  setText($('page-title'),pages[key]);
  if(key==='overview'&&$('message').dataset.kind==='config')message('');
  controls();$('content').scrollTop=pageScroll.get(key)||0;
  if(key==='preferences'){if(lastStatus)renderBackends(lastStatus);loadLogs();}
  if(key==='overview'){renderNodes();loadNodes();}
}
function updateFilter(attr,value){for(const button of document.querySelectorAll('[data-'+attr+'-filter]'))button.setAttribute('aria-pressed',String(button.dataset[attr+'Filter']===value));}
function syncChildren(parent,children){
  const wanted=new Set(children);
  for(let i=0;i<children.length;i++)if(parent.children[i]!==children[i])parent.insertBefore(children[i],parent.children[i]||null);
  for(const child of [...parent.children])if(!wanted.has(child))child.remove();
}
function nodeGroup(node){return ['shell','battery'].includes(node.group)?'shell':['cpu','gpu','ddr'].includes(node.group)?node.group:'other';}
function renderNodes(){
  setText($('node-note'),count(nodes.length)+' 个接口');
  const summary=$('node-summary'),tags=[];
  for(const [key,label] of [['shell','BAT'],['cpu','CPU'],['gpu','GPU'],['ddr','DDR']]){
    const total=nodes.filter(n=>nodeGroup(n)===key).length,tag=element('span',null,'node-tag tone-'+key);
    tag.append(element('span',label),element('strong',count(total)));tags.push(tag);
    const button=document.querySelector('[data-node-filter="'+key+'"]');button.setAttribute('aria-label',label+'，'+total+' 个接口');
  }
  summary.replaceChildren(...tags);summary.hidden=!$('node-content').hidden;
  setText(document.querySelector('[data-node-filter=all] .filter-count'),count(nodes.length));
  if($('node-content').hidden)return;
  const list=$('thermal-nodes'),top=list.scrollTop,rows=[];
  for(const n of nodes.filter(n=>nodeFilter==='all'||nodeGroup(n)===nodeFilter)){
    const row=element('div',null,'node-row tone-'+nodeGroup(n)),name=element('div',null,'node-name');
    name.append(element('strong',n.type||'未知接口'),element('small','/'+String(n.path||'').split('/').slice(-2).join('/')));name.title=n.path||'';
    const temp=number(n.temp_c),reading=element('strong',temp==null?(n.raw==null?'—':String(n.raw)+' · 原始值'):temp.toFixed(1)+'°','node-reading'+(temp==null?' raw':''));
    reading.setAttribute('aria-label',temp==null?'原始读数 '+(n.raw??'未知'):temp.toFixed(1)+'摄氏度');
    const state=element('span',n.modified?(n.method?.kind==='bind'?'已映射':'模拟值'):n.raw==null?'未读取':'读取','node-state'+(n.modified?' modified':''));
    row.append(name,reading,state);rows.push(row);
  }
  list.replaceChildren(...(rows.length?rows:[element('p','暂无匹配接口','empty')]));list.scrollTop=top;
}
async function loadNodes(){
  if(loadingNodes||busy||document.hidden||!viewOpen||currentPage!=='overview'||$('node-content').hidden||!CG.available())return;
  loadingNodes=true;const epoch=requestEpoch,generation=pollGeneration,identity=nodeIdentity;
  try{
    const result=await CG.call('thermal-nodes');
    if(!requestCurrent(epoch,generation)||currentPage!=='overview'||$('node-content').hidden||identity!==nodeIdentity)return;
    nodes=Array.isArray(result.nodes)?result.nodes:[];renderNodes();
  }catch(error){if(requestCurrent(epoch,generation)&&currentPage==='overview')setText($('node-note'),'接口读取失败：'+error.message);}
  finally{loadingNodes=false;}
}
function clearTelemetry(){
  nodes=[];nodeIdentity='';
  for(const id of ['temp','capacity','mounts','profile','round','detected','pid','diagnostic-round','diagnostic-errors','target-battery','target-cpu','target-gpu','target-ddr'])setText($(id),'—');
  $('capacity-unit').hidden=true;$('groups').replaceChildren();$('camera-hint').hidden=true;setText($('battery-note'),'等待最新接口读取');
  setText($('target-label'),'配置目标');$('diagnostic-errors').classList.remove('has-errors');
  setText($('maintenance-counts'),'等待运行统计');renderNodes();setText($('node-note'),'尚无接口数据');
}
function paintState(node,label,tone){setText(node,label);node.dataset.tone=tone;}
function render(data){
  lastStatus=data;
  if(data.device_info)setText($('oem-version'),[data.device_info.model,data.device_info.oem_version].filter(Boolean).join(' · '));
  const s=data.status||{},active=data.worker_alive||data.guardian_alive,fresh=!!data.fresh,idle=s.profile==='idle';
  const phase=fresh?(idle?'待机观察':s.phase==='partial'?'部分重试':'运行中'):data.worker_alive?'等待更新':data.guardian_alive?'等待重启':'已停止';
  const tone=fresh&&!idle?(s.phase==='partial'?'warn':'good'):active?'warn':'muted';
  setText($('core-state'),phase);$('core-badge').dataset.tone=tone;paintState($('diagnostic-state'),phase,tone);
  $('start').hidden=!!active;$('stop').hidden=!active;
  setText($('freshness'),number(data.age_ms)==null?'尚无记录':(data.age_ms/1000).toFixed(1)+' 秒前更新');
  setText($('runtime-mode'),fresh?(idle?'未应用预设':s.write_mode==='event'?'事件维护':'循环维护'):'等待连接');
  setText($('charge-state'),fresh?(chargeNames[s.charge_status]||'状态未知'):'—');
  const reason=fresh?(idle?(s.config?.charge_trigger?'等待充电，当前未应用运行时控制':'预设已关闭，核心保持观察'):''):(active?'守护进程继续维护，请稍后刷新':'点击启动核心恢复观察');
  setText($('reason'),reason);$('reason').hidden=!reason;
  const ops=Array.isArray(s.backends)?s.backends:[];
  setText($('backend-tab-count'),count(ops.length));setText(document.querySelector('[data-backend-filter=all] .filter-count'),count(ops.length));
  setText($('log-detail-state'),s.logging?.detailed_enabled?'详细日志已开启':'详细日志关闭');
  $('logging-error').hidden=!s.logging?.detail_error;setText($('logging-error'),s.logging?.detail_error?'详细日志记录失败：'+s.logging.detail_error:'');
  if(currentPage==='preferences')renderBackends(data);
  if(!fresh){clearTelemetry();if(data.last_worker_error)message('核心启动记录：'+data.last_worker_error,true);return;}
  const temperature=number(s.battery?.temperature_c),capacity=number(s.battery?.capacity);
  setText($('temp'),temperature==null?'—':temperature.toFixed(1));setText($('capacity'),capacity==null?'—':capacity);$('capacity-unit').hidden=capacity==null;
  setText($('battery-note'),'接口实时读数');setText($('profile'),profileNames[s.profile]||'—');setText($('round'),count(s.round));setText($('mounts'),count(s.mount_count));setText($('pid'),'PID '+s.pid);
  setText($('target-label'),idle?'目标未启用':'配置目标');
  for(const key of ['battery','cpu','gpu','ddr'])setText($('target-'+key),!idle&&number(s.targets?.[key])!=null?(s.targets[key]/1000)+'°':'—');
  const chips=[];
  for(const [family,label] of [['services','服务'],['shell','壳温'],['cpu','CPU'],['gpu','GPU'],['ddr','DDR'],['frequency','频率'],['cooling','Cooling']]){
    const group=ops.filter(o=>o.family===family),applied=group.filter(o=>o.state==='applied').length,failed=group.some(o=>['retrying','restore_pending'].includes(o.state));
    const tag=element('span',label+' '+(applied||'—'),'backend-chip tone-'+family+' '+(failed?'warn':applied?'on':''));
    tag.title=failed?'存在待重试操作':applied?'已应用 '+applied+' / '+group.length+' 项':'当前未应用';chips.push(tag);
  }
  $('groups').replaceChildren(...chips);
  const identity=[s.boot_id,s.pid,s.profile,s.config?.revision].join(':');
  if(identity!==nodeIdentity){nodes=[];nodeIdentity=identity;}
  nodes=Array.isArray(s.nodes)?s.nodes:nodes.length?nodes:Array.isArray(s.node_inventory)?s.node_inventory:[];
  setText($('detected'),count(nodes.length));if(currentPage==='overview')renderNodes();
  $('camera-hint').hidden=!s.smart_horae;
  if(s.smart_horae){
    const usage=s.camera?.usage,applied=ops.some(o=>o.id==='horae'&&o.state==='applied');
    setText($('camera-hint'),usage==='busy'?'Horae · 相机占用或不可用，保留服务':usage==='idle'?(applied?'Horae · 相机空闲，已禁用':'Horae · 等待空闲稳定'):'Horae · 相机状态未知，保留服务');
  }
  const failures=ops.filter(o=>['retrying','restore_pending'].includes(o.state)).length;
  setText($('diagnostic-round'),count(s.round));setText($('diagnostic-errors'),count(failures));$('diagnostic-errors').classList.toggle('has-errors',failures>0);
  setText($('maintenance-counts'),'节点写入 '+count(s.counters?.node_writes)+' · 映射更新 '+count(s.counters?.mask_writes)+' · 事件唤醒 '+count(s.control_listeners?.wakeups));
  if(s.config_error)message('配置读取失败，继续使用上次有效设置：'+s.config_error,true,'config_error');else if($('message').dataset.kind==='config_error')message('');
}

function backendGroup(op){
  if(op.id==='bouncing')return ['bouncing','Bouncing','frequency'];
  if(op.family==='cooling')return ['cooling','Cooling','cooling'];
  if(String(op.id).startsWith('shell_temp_'))return ['shell_temp','shell-temp','shell'];
  if(String(op.target).endsWith('/emul_temp'))return ['emul_temp','emul_temp','shell'];
  if(String(op.target).startsWith('/proc/game_opt/'))return ['game_opt','game_opt','frequency'];
  if(['omrg','migt'].includes(op.id))return [op.id,op.id.toUpperCase(),'frequency'];
  const names={cpu:'CPU 温度映射',gpu:'GPU 温度映射',ddr:'DDR 温度映射',services:'系统服务'};
  if(names[op.family])return [op.family,names[op.family],op.family];
  return [op.id||op.target,op.id||op.target||'其他后端',op.family||'other'];
}
function stateText(op){return op.state==='applied'&&['write_accepted_no_readback','write_accepted_readback_masked_by_switch'].includes(op.detail)?'已写入':stateNames[op.state]||op.state||'未知';}
function stateClass(ops){return ops.some(o=>['retrying','restore_pending'].includes(o.state))?'warn':ops.every(o=>o.state==='applied')?'good':'';}
function groupMode(group){
  if(group.items.every(o=>o.state==='restored'))return '已恢复';
  if(group.items.every(o=>!o.maintenance_mode))return group.items.some(o=>o.state==='restore_pending')?'等待恢复':'—';
  const modes=[...new Set(group.items.map(o=>o.maintenance_mode))];
  return modes.length===1?(group.key==='game_opt'&&modes[0]==='loop'?'周期续写':modeNames[modes[0]]||'—'):'混合维护';
}
function createBackendRecord(){
  const el=element('details',null,'backend-record'),summary=element('summary'),heading=element('span',null,'backend-title'),title=element('strong'),total=element('small'),mode=element('span',null,'backend-mode'),state=element('span',null,'backend-state'),body=element('div',null,'backend-body');
  heading.append(title,total);summary.append(heading,mode,state,icon('next'));el.append(summary,body);return {el,title,total,mode,state,body,items:new Map()};
}
function createBackendNode(){
  const el=element('section',null,'backend-node'),heading=element('div',null,'backend-node-heading'),title=element('strong'),state=element('span',null,'backend-state'),path=element('code',null,'backend-path'),values=element('dl',null,'backend-values'),counters=element('div',null,'backend-counters'),reason=element('p',null,'backend-reason'),detail=element('p',null,'backend-detail'),raw=element('details',null,'raw-fields'),pre=element('pre');
  raw.append(element('summary','完整字段'),pre);heading.append(title,state);el.append(heading,path,values,counters,reason,detail,raw);return {el,heading,title,state,path,values,pairs:new Map(),counters,reason,detail,pre};
}
function paintBackendNode(record,op,multiple){
  record.heading.hidden=!multiple;setText(record.title,String(op.target||op.id||'未知接口').split('/').slice(-2).join('/'));
  setText(record.state,stateText(op));record.state.className='backend-state '+stateClass([op]);setText(record.path,op.target||op.id);
  const pairs=[
    [op.can_verify?'目标 / 回读':'目标值',op.can_verify?String(op.desired??'—')+' / '+String(op.observed_after??'—'):String(op.desired??'—')],
    ['回读',op.can_verify===false?'无法完整核验':{matched:'符合目标',mismatched:'与目标不符',unreadable:'无法读取',not_checked:'未核验'}[op.verification]||'—'],
    ['最近处理',uptime(op.last_maintenance_ms)],
    ['本次处理',typeof op.write_performed==='boolean'?(op.write_performed?'已执行写入':'未执行写入'):'—']
  ];
  const fields=[];
  for(const [key,value] of pairs){let field=record.pairs.get(key);if(!field){const el=element('div'),dt=element('dt',key),dd=element('dd');el.append(dt,dd);field={el,dd};record.pairs.set(key,field);}setText(field.dd,value);field.dd.classList.toggle('good',key==='回读'&&op.verification==='matched');fields.push(field.el);}
  syncChildren(record.values,fields);
  const checks=[['verification_checks','核验'],['repair_writes','修复'],['skipped_writes','省略']].filter(([key])=>Number.isFinite(op[key])).map(([key,label])=>element('span',label+' '+count(op[key])+' 次'));
  record.counters.replaceChildren(...checks);record.counters.hidden=!checks.length;
  const source={property_wait:'服务状态通知',node_inotify:'inotify','mount_poll+inotify':'挂载与文件通知','thermal_tracepoint+thermal_netlink+inotify':'thermal / netlink / inotify'}[op.event_source]||op.event_source;
  const reason=[source,op.maintenance_reason].filter(Boolean).join(' · ');setText(record.reason,reason);record.reason.hidden=!reason;
  const detail={write_accepted_no_readback:'接口写入已完成；该接口不提供可比对的回读。',write_accepted_readback_masked_by_switch:'写入已完成；显示值受禁限开关影响，不能验证内部请求。'}[op.detail]||op.detail;
  setText(record.detail,detail);record.detail.hidden=!detail;setText(record.pre,JSON.stringify(op,null,2));
}
function renderBackends(data){
  if(!diagnosticExpanded||diagnosticTab!=='backends'||currentPage!=='preferences')return;
  const s=data.status||{},ops=Array.isArray(s.backends)?s.backends:[],root=$('operations'),scroll=root.scrollTop;
  const error=s.listeners?.power_error||s.listeners?.file_error||s.control_listener_error;
  const note=error?'事件监听正在重连：'+error:!data.fresh?'以下为最近一次记录，当前并非实时运行状态。':'';
  $('backend-note').hidden=!note;setText($('backend-note'),note);
  const groups=new Map();
  for(const op of ops){const [key,name,family]=backendGroup(op);if(!groups.has(key))groups.set(key,{key,name,family,items:[]});groups.get(key).items.push(op);}
  const order=['bouncing','cooling','shell_temp','emul_temp','game_opt','cpu','gpu','ddr','services','omrg','migt'];
  const selected=[...groups.values()].filter(g=>backendFilter==='all'||(backendFilter==='temperature'?['shell','cpu','gpu','ddr'].includes(g.family):g.family===backendFilter));
  selected.sort((a,b)=>(order.includes(a.key)?order.indexOf(a.key):99)-(order.includes(b.key)?order.indexOf(b.key):99));
  const children=[];
  for(const group of selected){
    let record=backendRecords.get(group.key);if(!record){record=createBackendRecord();backendRecords.set(group.key,record);}
    record.el.className='backend-record tone-'+(['cpu','gpu','ddr','shell','services','frequency','cooling'].includes(group.family)?group.family:'other');
    setText(record.title,group.name);setText(record.total,group.items.length>1?group.items.length+(group.family==='cooling'?' 个目标':' 项'):'');
    setText(record.mode,groupMode(group));record.mode.className='backend-mode '+(group.items.every(o=>o.maintenance_mode==='dormant')?'dormant':group.items.some(o=>o.maintenance_mode==='event_check')?'fallback':group.items.every(o=>o.maintenance_mode==='loop')?'periodic':'');
    const states=[...new Set(group.items.map(stateText))];setText(record.state,states.length===1?states[0]:'多种状态');record.state.className='backend-state '+stateClass(group.items);
    const bodies=[];
    for(const op of group.items){const key=op.id||op.target;let item=record.items.get(key);if(!item){item=createBackendNode();record.items.set(key,item);}paintBackendNode(item,op,group.items.length>1);bodies.push(item.el);}
    syncChildren(record.body,bodies);for(const key of record.items.keys())if(!group.items.some(o=>(o.id||o.target)===key))record.items.delete(key);children.push(record.el);
  }
  for(const key of backendRecords.keys())if(!groups.has(key))backendRecords.delete(key);
  syncChildren(root,children.length?children:[element('p',ops.length?'暂无匹配后端':s.profile==='idle'?'当前处于待机，未启用控制后端。':s.phase==='stopped'?'核心已停止，目前没有待恢复的操作。':'尚无后端记录，启动核心后会显示。','empty')]);root.scrollTop=scroll;
}

const logNames={detailed_logging_changed:'详细日志设置',cooling_listener_changed:'Cooling 监听状态',maintenance_mode_changed:'维护模式更新',horae_camera_restore:'相机优先恢复 Horae',charge_listener:'充电事件监听',charge_changed:'充电状态变化',maintenance:'后端维护',profile_changed:'预设切换',config_changed:'设置已保存',camera_state:'智能 Horae',worker_exit:'核心进程退出',watchdog_wait:'等待自动重启',spawn_failed:'启动失败',worker_stop_timeout:'停止超时',status_write_failed:'状态记录失败'};
function isError(e){return /failed|timeout|_error$/.test(e.event||'')||Number(e.detail?.failures)>0||!!e.detail?.error||(e.event==='worker_exit'&&!e.detail?.requested);}
function logTone(e){return isError(e)?'error':/camera|horae/.test(e.event||'')?'camera':/cooling/.test(e.event||'')?'cooling':'config';}
function logSummary(e){
  const d=e.detail;if(typeof d==='string')return d;if(!d||typeof d!=='object')return '查看完整记录';if(d.error)return String(d.error);
  switch(e.event){
    case 'detailed_logging_changed':return d.enabled?'已开启详细日志 · 按大小轮转':'已关闭详细日志';
    case 'cooling_listener_changed':return d.transport?.ready?'监听就绪 · 事件驱动维护':d.transport?.error||'监听尚未就绪 · 局部周期核验';
    case 'maintenance_mode_changed':return '纯事件 '+count(d.event_backends)+' 项 · 周期 '+count(d.periodic_backends)+' 项 · 休眠 '+count(d.dormant_backends)+' 项';
    case 'horae_camera_restore':return '服务状态 '+(d.service_state||'未知')+' · '+(d.backends?.some(b=>b.state==='restore_pending')?'等待恢复确认':'查看恢复结果');
    case 'charge_listener':return d.mode==='uevent'?'使用充电事件通知':'充电事件监听暂不可用';
    case 'charge_changed':return (chargeNames[d.from]||d.from||'未知')+' → '+(chargeNames[d.to]||d.to||'未知');
    case 'maintenance':return '轮次 '+count(d.round)+' · 处理 '+count(d.due_backends)+' 项 · 待重试 '+count(d.failures);
    case 'profile_changed':return (profileNames[d.from]||'初始状态')+' → '+(profileNames[d.to]||d.to||'未知');
    case 'config_changed':return '配置修订 '+count(d.revision)+' 已载入';
    case 'camera_state':return ({busy:'相机占用或不可用，保留 Horae',idle:'相机空闲，等待稳定后禁用 Horae',unknown:'相机状态未知，保留 Horae'}[d.usage]||d.reason||'相机状态更新');
    case 'worker_exit':return d.requested?'核心已按请求退出':'退出代码 '+String(d.code??'未知');
    case 'watchdog_wait':return count(d.seconds)+' 秒后尝试重启核心';
    default:return JSON.stringify(d);
  }
}
function renderLogs(){
  const list=$('log-list'),scroll=list.scrollTop,occurrences=new Map(),children=[],wanted=new Set();
  for(const e of logEntries.slice().reverse()){
    const match=logFilter==='all'||logFilter==='error'&&isError(e)||logFilter==='camera'&&['camera_state','horae_camera_restore'].includes(e.event)||logFilter==='config'&&['config_changed','profile_changed'].includes(e.event);
    const encoded=JSON.stringify(e),ordinal=occurrences.get(encoded)||0;occurrences.set(encoded,ordinal+1);const key=encoded+'#'+ordinal;wanted.add(key);
    if(!match)continue;
    let record=logRecords.get(key);
    if(!record){
      record=element('details',null,'log-record tone-'+logTone(e));const head=element('summary'),copy=element('span',null,'log-copy');
      copy.append(element('strong',logNames[e.event]||e.event||'运行记录'),element('small',logSummary(e)));
      head.append(element('time',uptime(e.boottime_ms)),element('span',null,'log-dot'),copy,icon('next'));
      record.append(head,element('pre',JSON.stringify(e,null,2)));logRecords.set(key,record);
    }
    children.push(record);
  }
  for(const key of logRecords.keys())if(!wanted.has(key))logRecords.delete(key);
  syncChildren(list,children.length?children:[element('p','暂无此类记录','empty')]);list.scrollTop=scroll;
  for(const id of ['log-count','log-tab-count'])setText($(id),logsLoaded?count(logEntries.length):'—');
  setText($('log-total'),logsLoaded?'最近 '+count(logEntries.length)+' 条':'尚未加载');
}
async function loadLogs(){
  if(!diagnosticExpanded||diagnosticTab!=='logs'||loadingLogs||busy||document.hidden||!viewOpen||currentPage!=='preferences'||!CG.available())return;
  loadingLogs=true;const epoch=requestEpoch,generation=pollGeneration;
  try{
    const result=await CG.call('logs');
    if(!requestCurrent(epoch,generation)||currentPage!=='preferences'||!diagnosticExpanded||diagnosticTab!=='logs')return;
    logEntries=Array.isArray(result.entries)?result.entries:[];
    for(const [key,label] of [['worker_error','核心启动记录'],['guardian_error','守护进程记录']])if(result[key])logEntries.push({event:label,detail:{error:result[key]}});
    logsLoaded=true;renderLogs();
  }catch(error){if(requestCurrent(epoch,generation)&&currentPage==='preferences'&&diagnosticExpanded&&diagnosticTab==='logs')$('log-list').replaceChildren(element('p','日志读取失败：'+error.message,'empty'));}
  finally{loadingLogs=false;}
}
function diagnostics(tab,expanded=true){
  diagnosticTab=tab;diagnosticExpanded=expanded;
  for(const key of ['logs','backends']){
    const selected=key===tab;$('diagnostic-tab-'+key).setAttribute('aria-selected',String(selected));$('diagnostic-tab-'+key).tabIndex=selected?0:-1;
  }
  $('log-content').hidden=!expanded||tab!=='logs';$('backend-content').hidden=!expanded||tab!=='backends';
  $('toggle-diagnostics').setAttribute('aria-expanded',String(expanded));$('toggle-diagnostics').setAttribute('aria-label',expanded?'收起诊断面板':'展开诊断面板');
  if(expanded&&tab==='logs'){if(logsLoaded)renderLogs();loadLogs();}else if(expanded&&lastStatus)renderBackends(lastStatus);
}
async function refresh(){
  if(refreshing||busy||document.hidden||!viewOpen||!CG.available())return;
  refreshing=true;const epoch=requestEpoch,generation=pollGeneration;
  try{
    const data=await CG.call('ui-status');if(!requestCurrent(epoch,generation)||busy)return;
    if(!dirty&&data.config)populate(data.config);if($('message').dataset.kind==='connection')message('');render(data);loadNodes();
  }catch(error){
    if(!requestCurrent(epoch,generation))return;clearTelemetry();
    if(lastStatus){lastStatus={...lastStatus,fresh:false};renderBackends(lastStatus);}
    setText($('core-state'),'连接失败');$('core-badge').dataset.tone='warn';paintState($('diagnostic-state'),'状态未知','warn');setText($('runtime-mode'),'等待连接');setText($('charge-state'),'状态未知');message(error.message,true,'connection');
  }finally{refreshing=false;controls();}
}
async function save(event){
  event.preventDefault();if(busy||!config||!dirty)return;
  requestEpoch++;busy=true;controls();
  try{
    const next={...config};for(const key of tempKeys)next[key]=Math.round(Number($(key).value)*1000);
    for(const key of ['global_enabled','horae_stop','charge_trigger','charge_horae_enabled','detailed_logging'])next[key]=$(key).checked;
    next.charge_horae_mode=document.querySelector('[name=charge_horae_mode]:checked').value;next.write_mode=document.querySelector('[name=write_mode]:checked').value;
    const result=await CG.call('configure-hex',CG.hex({expected_revision:config.revision,config:next}));populate(result.config);message('设置已保存',false,'config');
  }catch(error){message(error.message.includes('revision_conflict')?'配置已在其他位置更新，请撤销更改后重新编辑。':error.message,true);}
  finally{busy=false;controls();await refresh();}
}
function exportLog(result){
  if(!result.public_path){
    const blob=new Blob([result.content],{type:'text/plain;charset=utf-8'}),url=URL.createObjectURL(blob),link=document.createElement('a');link.href=url;link.download=result.filename;document.body.append(link);link.click();link.remove();setTimeout(()=>URL.revokeObjectURL(url),10000);
  }
  const destination=result.public_path||result.path;$('export-result').hidden=false;
  setText($('export-result'),result.public_path?'已保存到下载目录：'+result.public_path:'日志已生成并发起下载'+(destination?'。本机文件：'+destination:'：'+result.filename));message('日志文件已生成');
}
async function action(verb){
  if(busy)return;requestEpoch++;busy=true;controls();
  try{const result=await CG.call(verb,undefined,45000);if(verb==='export')exportLog(result);else message(verb==='stop'?'核心已停止，自有运行时操作已恢复':'核心已启动');}
  catch(error){message(error.message,true);}finally{busy=false;controls();await refresh();}
}

for(const slider of document.querySelectorAll('input[type=range]')){
  const wrap=element('div',null,'range-wrap'),surface=element('div',null,'range-gesture');surface.setAttribute('aria-hidden','true');slider.before(wrap);wrap.append(slider,surface);
  let gesture=null;
  function setValue(value){const min=Number(slider.min),max=Number(slider.max),step=Number(slider.step)||1,next=Math.round((Math.max(min,Math.min(max,value))-min)/step)*step+min;if(next!==Number(slider.value)){slider.value=String(next);slider.dispatchEvent(new Event('input',{bubbles:true}));}}
  surface.addEventListener('pointerdown',event=>{
    if(slider.disabled||event.button!==0)return;gesture={id:event.pointerId,x:event.clientX,y:event.clientY,value:Number(slider.value),touch:event.pointerType==='touch',mode:event.pointerType==='touch'?null:'adjust'};surface.setPointerCapture(event.pointerId);
    if(!gesture.touch){event.preventDefault();slider.focus({preventScroll:true});const box=slider.getBoundingClientRect();setValue(Number(slider.min)+(event.clientX-box.x-8)/Math.max(1,box.width-16)*(Number(slider.max)-Number(slider.min)));gesture.value=Number(slider.value);}
  });
  surface.addEventListener('pointermove',event=>{
    if(!gesture||gesture.id!==event.pointerId||slider.disabled)return;const dx=event.clientX-gesture.x,dy=event.clientY-gesture.y;
    if(!gesture.mode){if(Math.max(Math.abs(dx),Math.abs(dy))<12)return;gesture.mode=Math.abs(dx)>Math.abs(dy)*1.5?'adjust':'scroll';}
    if(gesture.mode!=='adjust')return;event.preventDefault();setValue(gesture.value+dx/Math.max(1,slider.getBoundingClientRect().width-16)*(Number(slider.max)-Number(slider.min)));
  });
  const finish=event=>{if(gesture?.id===event.pointerId){if(gesture.mode==='adjust')slider.dispatchEvent(new Event('change',{bubbles:true}));gesture=null;}};
  surface.addEventListener('pointerup',finish);surface.addEventListener('pointercancel',finish);surface.addEventListener('lostpointercapture',()=>{gesture=null;});
}
for(const button of document.querySelectorAll('[data-adjust]'))button.onclick=()=>{
  if(button.disabled)return;const slider=$(button.dataset.target),next=Math.max(Number(slider.min),Math.min(Number(slider.max),Number(slider.value)+Number(button.dataset.adjust)));
  if(next===Number(slider.value))return;slider.value=String(next);slider.dispatchEvent(new Event('input',{bubbles:true}));slider.dispatchEvent(new Event('change',{bubbles:true}));
};
$('settings').addEventListener('input',event=>{if(event.target.name==='theme')return;dirty=true;labels();controls();});$('settings').addEventListener('submit',save);
function tabKeys(buttons,select){for(const [i,button] of buttons.entries())button.addEventListener('keydown',event=>{
  let index;if(event.key==='ArrowRight')index=(i+1)%buttons.length;else if(event.key==='ArrowLeft')index=(i+buttons.length-1)%buttons.length;else if(event.key==='Home')index=0;else if(event.key==='End')index=buttons.length-1;else return;
  event.preventDefault();buttons[index].focus();select(index);
});}
const pageKeys=Object.keys(pages);for(const key of pageKeys)$('tab-'+key).onclick=()=>page(key);tabKeys(pageKeys.map(key=>$('tab-'+key)),index=>page(pageKeys[index]));
for(const radio of document.querySelectorAll('[name=theme]')){radio.checked=radio.value===CGTheme.get();radio.onchange=()=>{if(radio.checked){CGTheme.set(radio.value);labels();}};}
function displayScale(value){
  if(value!==undefined)CGDisplay.set(value);
  const scale=CGDisplay.get();setText($('dpi-value'),scale+'%');
  $('dpi-decrease').disabled=scale<=95;$('dpi-increase').disabled=scale>=115;
  $('dpi-reset').disabled=scale===105;
}
$('dpi-decrease').onclick=()=>displayScale(CGDisplay.get()-5);
$('dpi-increase').onclick=()=>displayScale(CGDisplay.get()+5);
$('dpi-reset').onclick=()=>displayScale(105);displayScale();
for(const verb of ['start','stop','export'])$(verb).onclick=()=>action(verb);
for(const tab of ['logs','backends'])$('diagnostic-tab-'+tab).onclick=()=>diagnostics(tab);
tabKeys(['logs','backends'].map(key=>$('diagnostic-tab-'+key)),index=>diagnostics(index?'backends':'logs'));
$('toggle-diagnostics').onclick=()=>diagnostics(diagnosticTab,!diagnosticExpanded);
$('overview-refresh').onclick=refresh;$('refresh-logs').onclick=()=>{refresh();loadLogs();};$('refresh-backends').onclick=refresh;
$('toggle-nodes').onclick=()=>{
  const expanded=$('node-content').hidden;$('node-content').hidden=!expanded;$('toggle-nodes').setAttribute('aria-expanded',String(expanded));$('toggle-nodes').setAttribute('aria-label',expanded?'收起温度接口':'展开温度接口');renderNodes();if(expanded)loadNodes();
};
for(const button of document.querySelectorAll('[data-node-filter]'))button.onclick=()=>{nodeFilter=button.dataset.nodeFilter;updateFilter('node',nodeFilter);$('thermal-nodes').scrollTop=0;renderNodes();};
for(const button of document.querySelectorAll('[data-log-filter]'))button.onclick=()=>{logFilter=button.dataset.logFilter;updateFilter('log',logFilter);$('log-list').scrollTop=0;renderLogs();};
for(const button of document.querySelectorAll('[data-backend-filter]'))button.onclick=()=>{backendFilter=button.dataset.backendFilter;updateFilter('backend',backendFilter);$('operations').scrollTop=0;if(lastStatus)renderBackends(lastStatus);};
$('discard').onclick=async()=>{
  if(busy)return;requestEpoch++;busy=true;controls();try{populate(await CG.call('config'));message('已载入保存的设置',false,'config');}catch(error){message(error.message,true);}finally{busy=false;controls();}
};
function pausePolling(){pollingActive=false;pollGeneration++;requestEpoch++;if(pollTimer!==null)clearTimeout(pollTimer);pollTimer=null;}
function resumePolling(){
  if(document.hidden||!viewOpen||!CG.available()){pausePolling();return;}if(pollingActive)return;pollingActive=true;
  const generation=pollGeneration;
  const tick=async()=>{
    if(generation!==pollGeneration||document.hidden||!viewOpen)return;await refresh();
    if(generation!==pollGeneration||document.hidden||!viewOpen)return;await loadLogs();
    if(generation===pollGeneration&&!document.hidden&&viewOpen)pollTimer=setTimeout(tick,2500);
  };tick();
}
document.addEventListener('visibilitychange',resumePolling);
window.addEventListener('pagehide',()=>{viewOpen=false;pausePolling();});window.addEventListener('pageshow',()=>{viewOpen=true;resumePolling();});
controls();labels();
if(CG.available())resumePolling();else{setText($('core-state'),'离线');setText($('runtime-mode'),'未连接');setText($('reason'),'请在模块管理器中打开');$('reason').hidden=false;}
})();
