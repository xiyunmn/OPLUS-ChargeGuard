(function(){
'use strict';
const $=id=>document.getElementById(id),ns='http://www.w3.org/2000/svg';
const metrics={net_power_w:{label:'电池净功率',unit:'W'},current_ma:{label:'电池电流',unit:'mA'},voltage_mv:{label:'电芯电压',unit:'mV'},soc:{label:'电量',unit:'%'}};
let opened=false,viewOpen=true,view='current',returnView='current',viewHistory=false,dialogHistory=false,dialogKind='',backdropPress=false,returnAfterDialog=false;
let enabled=true,runtime=null,recordingError=null,listLoaded=false,listBusy=false,detailBusy=false,storageBusy=false,deleting=false;
let active=null,latestSession=null,items=[],total=0,aggregate={},storage=null,measurement=null,activeDetail=null,selectedDetail=null,selectedId=null;
let range='7',filter={protocols:[],completeness:'all'},metric='net_power_w',period='full',cursorElapsed=null,windowStart=0,windowEnd=null;
let generation=0,filterGeneration=0,windowGeneration=0,pollTimer=null,windowTimer=null,job=null,exportStarting=false,exportPolling=false;
const dialog=$('statistics-dialog'),body=$('statistics-dialog-body');
function el(tag,text,cls){const node=document.createElement(tag);if(text!=null)node.textContent=text;if(cls)node.className=cls;return node;}
function svgIcon(name){const node=document.createElementNS(ns,'svg'),use=document.createElementNS(ns,'use');node.setAttribute('class','icon');node.setAttribute('aria-hidden','true');use.setAttribute('href','#i-'+name);node.append(use);return node;}
function text(id,value){const next=String(value??'—');if($(id).textContent!==next)$(id).textContent=next;}
function number(value){return value==null||value===''||!Number.isFinite(Number(value))?null:Number(value);}
function format(value,digits=1){const n=number(value);return n==null?'—':n.toFixed(digits).replace(/\.0$/,'');}
function soc(value){return number(value)==null?'—':format(value,0)+'%';}
function duration(value){const n=number(value);if(n==null)return '—';const minutes=Math.floor(Math.max(0,n)/60000);return minutes>=60?Math.floor(minutes/60)+'小时'+(minutes%60?minutes%60+'分':''):minutes+'分钟';}
function elapsed(value){const seconds=Math.floor(Math.max(0,Number(value)||0)/1000);return Math.floor(seconds/60)+':'+String(seconds%60).padStart(2,'0');}
function clock(value){return number(value)==null?'—':new Date(value).toLocaleTimeString('zh-CN',{hour:'2-digit',minute:'2-digit',hour12:false});}
function date(value,year=false){if(number(value)==null)return '—';const d=new Date(value);return (year?d.getFullYear()+'年':'')+(d.getMonth()+1)+'月'+d.getDate()+'日';}
function dateInput(value){const d=new Date(value);return [d.getFullYear(),String(d.getMonth()+1).padStart(2,'0'),String(d.getDate()).padStart(2,'0')].join('-');}
function bytes(value){const n=number(value);if(n==null)return '—';return n>=1048576?format(n/1048576)+' MB':n>=1024?format(n/1024)+' KB':format(n,0)+' B';}
function protocol(value){const key=String(value||'').toLowerCase();return ({supervooc:'SVOOC',svooc:'SVOOC',vooc:'VOOC',pps:'PPS',pd:'PD',ufcs:'UFCS',qc:'QC',usb:'USB',dcp:'DCP',other:'其他',unknown:'协议未知'})[key]||value||'协议未知';}
function protocols(summary){return (summary?.protocols||[]).map(protocol).join(' → ')||'协议未知';}
function currentData(){return view==='detail'?selectedDetail:activeDetail;}
function currentSession(){return active||latestSession;}
function currentSummary(){return currentData()?.summary||(view==='detail'?null:currentSession());}
function live(){return opened&&viewOpen&&!document.hidden&&CG.available();}
function request(value){return CG.call('statistics-hex',CG.hex(value));}
function notice(value,bad=false){const node=$('statistics-notice');node.hidden=!value;node.classList.toggle('error',bad);node.replaceChildren();if(value)node.textContent=value;}
function groupButtons(selector,key,attribute='aria-pressed'){for(const button of document.querySelectorAll(selector))button.setAttribute(attribute,String(button.dataset.statRange===key||button.dataset.statMetric===key||button.dataset.statPeriod===key));}
function selectedFilter(){
  const result={...filter,protocols:[...filter.protocols]};
  if(range!=='custom'){const end=new Date();end.setHours(23,59,59,999);const start=new Date();start.setHours(0,0,0,0);start.setDate(start.getDate()-Number(range)+1);result.from_ms=start.getTime();result.to_ms=end.getTime();}
  return result;
}
function filterLabel(){const labels=[];if(filter.protocols.length)labels.push(filter.protocols.map(protocol).join(' / '));if(filter.start_soc_max!=null)labels.push('起始≤'+filter.start_soc_max+'%');if(filter.end_soc_min!=null)labels.push('结束≥'+filter.end_soc_min+'%');if(filter.completeness!=='all')labels.push(filter.completeness==='complete'?'完整':'部分');return labels.join(' · ')||'筛选';}
function header(){
  if(!opened){$('statistics-settings').hidden=true;$('statistics-back').hidden=true;return;}
  text('page-title',view==='settings'?'记录设置':view==='detail'?'充电详情':'充电统计');
  $('statistics-settings').hidden=view==='settings'||view==='detail';$('statistics-back').hidden=view!=='settings'&&view!=='detail';
}
function present(){
  header();$('statistics-tabs').hidden=view==='settings'||view==='detail';
  $('statistics-current').hidden=!['current','detail'].includes(view);$('statistics-history').hidden=view!=='history';$('statistics-record-settings').hidden=view!=='settings';
  for(const key of ['current','history']){const button=$('statistics-tab-'+key);button.setAttribute('aria-selected',String(view===key));button.tabIndex=view===key?0:-1;}
  $('statistics-chart-period').hidden=view==='detail';$('statistics-window').hidden=view!=='detail';$('statistics-detail-actions').hidden=view!=='detail';
  renderSession();if(view==='history')renderHistory();if(view==='settings')renderStorage();
}
function setView(next,push=true){
  if(next===view)return;
  if(next==='settings'||next==='detail'){
    returnView=['current','history'].includes(view)?view:returnView;
    if(push&&!viewHistory){history.pushState({...history.state,chargeGuardStatisticsView:true},'');viewHistory=true;}
  }
  view=next;cursorElapsed=null;windowStart=0;windowEnd=null;windowGeneration++;present();$('content').scrollTop=0;
  if(live()){if(next==='settings')loadStorage();else if(next==='detail')loadDetail(true);else loadList();}
  schedule();
}
function leaveSubview(){if(viewHistory&&history.state?.chargeGuardStatisticsView){history.back();return;}viewHistory=false;setView(returnView,false);}
function stop(){generation++;if(pollTimer!==null)clearTimeout(pollTimer);pollTimer=null;if(windowTimer!==null)clearTimeout(windowTimer);windowTimer=null;}
function schedule(delay=5000){
  if(pollTimer!==null)clearTimeout(pollTimer);pollTimer=null;if(!live())return;
  pollTimer=setTimeout(async()=>{pollTimer=null;if(!live())return;
    if(job&&['queued','running'].includes(job.state))await pollExport();
    if(!live())return;
    if(view==='current')await loadList(false,true);else if(view==='detail'&&selectedDetail?.summary?.ended_unix_ms==null)await loadDetail();
    schedule(job&&['queued','running'].includes(job.state)?1000:5000);
  },delay);
}
async function loadList(append=false,quiet=false){
  if(!live()||listBusy)return;listBusy=true;const token=generation,query=filterGeneration,offset=append?items.length:0;
  $('statistics-load-more').disabled=true;
  try{
    const result=await request({action:'list',filter:selectedFilter(),offset});if(!live()||token!==generation||query!==filterGeneration)return;
    active=result.active||null;latestSession=result.latest||null;enabled=result.enabled!==false;recordingError=result.error||null;measurement=result.measurement||measurement;aggregate=result.aggregate||{};total=Number(result.total)||0;
    if(append)items.push(...(result.items||[]));else if(quiet&&items.length>20){const first=result.items||[],ids=new Set(first.map(item=>item.id));items=[...first,...items.filter(item=>!ids.has(item.id))];}else items=result.items||[];
    listLoaded=true;const current=currentSession();if(!current||activeDetail?.summary?.id!==current.id)activeDetail=null;else activeDetail.summary=current;
    if(!quiet){notice('');if(job&&!dialog.open)showJobNotice();}renderHistory();if(view==='current'){renderSession();loadDetail();}
  }catch(error){if(live()&&token===generation){notice('统计读取失败：'+error.message,true);if(!listLoaded){text('statistics-empty-title','暂时无法读取');text('statistics-empty-description','请稍后重新打开统计页');}}}
  finally{listBusy=false;$('statistics-load-more').disabled=false;if(live()&&(query!==filterGeneration||token!==generation&&!listLoaded))loadList();}
}
async function loadDetail(reset=false,windowed=false){
  const id=view==='detail'?selectedId:currentSession()?.id;if(!id||!live()||detailBusy)return;
  detailBusy=true;const token=generation,windowToken=windowGeneration,isDetail=view==='detail',old=isDetail?selectedDetail:activeDetail;
  const args={action:'detail',id};
  if(!reset&&old?.summary?.id===id)args.cursor=old.cursor;
  if(windowed){args.from_elapsed_ms=windowStart;args.to_elapsed_ms=windowEnd;args.max_points=1200;delete args.cursor;}
  try{
    const result=await request(args);if(!live()||token!==generation||id!==(isDetail?selectedId:currentSession()?.id)||windowed&&windowToken!==windowGeneration)return;
    const incremental=!reset&&!windowed&&!result.reset&&old?.summary?.id===id;
    if(incremental){const last=old.points.at(-1)?.seq;result.points=old.points.concat((result.points||[]).filter(point=>last==null||point.seq>last));result.events=old.events.concat(result.events||[]);}
    result.points=result.points||[];result.events=result.events||[];
    if(isDetail)selectedDetail=result;else activeDetail=result;
    renderSession();
  }catch(error){if(live()&&token===generation)notice('曲线读取失败：'+error.message,true);}
  finally{detailBusy=false;if(live()&&['current','detail'].includes(view)){const target=view==='detail'?selectedId:currentSession()?.id;if(target&&(target!==id||(view==='detail')!==isDetail||token!==generation))loadDetail(true);else if(view==='detail'&&windowToken!==windowGeneration)loadDetail(true,true);}}
}
async function loadStorage(){
  if(!live()||storageBusy)return;storageBusy=true;const token=generation;
  try{const result=await request({action:'storage'});if(!live()||token!==generation)return;storage=result;measurement=result.measurement||measurement;renderStorage();}
  catch(error){if(live()&&token===generation)notice('本地数据读取失败：'+error.message,true);}
  finally{storageBusy=false;if(live()&&view==='settings'&&token!==generation)loadStorage();}
}
function measurementCaption(source){return '电池净功率 · '+(source?.cell_count===2?'双芯折算':source?.cell_count?'电芯口径':'口径待确认');}
function renderStorage(){text('statistics-storage-count',storage?storage.sessions+'条':'—');text('statistics-storage-bytes',bytes(storage?.bytes));text('statistics-storage-dates',storage?.oldest_unix_ms?date(storage.oldest_unix_ms)+'—'+date(Date.now()):'暂无历史记录');$('statistics-clear').disabled=deleting||!CG.available()||!storage||storage.sessions<=storage.active;$('statistics-export-all').disabled=exportStarting||!CG.available()||!storage?.sessions;$('statistics-settings-measurement').querySelector('small').textContent=measurementCaption(measurement);}
function sessionState(summary){
  if(summary.ended_unix_ms!=null)return summary.complete?{text:'已结束',tone:'muted'}:{text:'部分记录',tone:'warn'};
  const battery=summary.latest?.status;
  if(battery==='Full')return {text:'已充满',tone:'good'};
  if(!enabled)return {text:'记录已关闭',tone:'muted'};
  if(runtime&&!(runtime.worker_alive||runtime.guardian_alive))return {text:'记录已暂停',tone:'warn'};
  if(summary.status==='partial'||summary.status==='error')return {text:'部分记录',tone:'warn'};
  return {text:battery==='Charging'?'充电中':battery==='Not charging'?'充电暂停':'记录中',tone:battery==='Charging'?'good':'muted'};
}
function renderSession(){
  if(!['current','detail'].includes(view))return;
  const data=currentData(),summary=currentSummary();$('statistics-session').hidden=!summary;$('statistics-empty').hidden=!!summary;
  if(!summary){
    const stopped=runtime&&!(runtime.worker_alive||runtime.guardian_alive);
    text('statistics-empty-title',recordingError?'记录异常':!enabled?'自动记录已关闭':stopped?'记录已暂停':listLoaded?'暂无充电记录':'等待充电记录');
    text('statistics-empty-description',recordingError||(!CG.available()?'请在模块管理器中打开':!enabled?'开启后记录之后的充电过程':stopped?'启动模块后继续自动记录':listLoaded?'连接充电器后自动记录':'正在读取本机数据…'));return;
  }
  const latest=summary.latest||data?.points.at(-1),state=sessionState(summary);
  $('statistics-session-label').hidden=view==='detail'||!!active;
  text('statistics-protocol',summary.ended_unix_ms==null?protocol(latest?.protocol??summary.protocols?.at(-1)):protocols(summary));text('statistics-session-state',state.text);$('statistics-session-state').dataset.tone=state.tone;
  $('statistics-measurement').querySelector('small').textContent=measurementCaption(summary.measurement||measurement);
  $('statistics-session-time').hidden=view!=='detail';text('statistics-session-time',date(summary.started_unix_ms)+' '+clock(summary.started_unix_ms)+'—'+(summary.ended_unix_ms==null?'现在':clock(summary.ended_unix_ms))+' · 连接 '+duration(summary.elapsed_ms));
  text('statistics-power',format(latest?.net_power_w));text('statistics-temperature',format(latest?.temp_c));
  text('statistics-soc',soc(summary.start_soc)+' → '+soc(summary.end_soc??latest?.soc));text('statistics-duration','充电 '+duration(summary.charging_ms));
  $('statistics-soc-fill').style.width=Math.max(0,Math.min(100,Number(summary.end_soc??latest?.soc)||0))+'%';
  const gaps=(data?.points||[]).some(point=>point.gap_before),partial=summary.ended_unix_ms!=null&&!summary.complete;
  const notes=[];if(partial)notes.push('部分记录，可能未覆盖整个充电过程');if(gaps)notes.push('未连续采样的区间已断开');if(view==='current'&&recordingError)notes.push('记录已暂停：'+recordingError);else if(view==='current'&&!enabled)notes.push('自动记录已关闭，当前不会新增记录');else if(view==='current'&&runtime&&!(runtime.worker_alive||runtime.guardian_alive))notes.push('模块已停止，当前不会继续记录');
  const note=notes.join('；');$('statistics-session-note').hidden=!note;text('statistics-session-note',note);
  text('statistics-peak',format(summary.power_peak_w));text('statistics-average',format(summary.power_avg_w));text('statistics-max-temperature',format(summary.temp_max_c));
  $('statistics-delete-session').hidden=summary.ended_unix_ms==null;$('statistics-delete-session').disabled=deleting;
  $('statistics-export-session').disabled=exportStarting;
  $('statistics-events-card').hidden=view!=='detail';if(view==='detail')renderEvents(data?.events||[],summary);
  renderCharts();
}
function renderEvents(events,summary){
  const names={session_start:'开始记录',power_state:'充电状态变化',protocol_changed:'协议变化',config_changed:'充电设置变化',data_gap:'数据缺失',session_end:'记录结束'};
  const nodes=[];
  for(const event of events){const node=el('div',null,'stats-event'),copy=el('div',names[event.kind]||'充电状态变化'),time=el('time',clock(event.unix_ms));time.dateTime=new Date(event.unix_ms).toISOString();
    const detail=event.detail;let desc=null;
    if(event.kind==='power_state'){copy.textContent=detail?.status==='Full'?'已充满':detail?.status==='Charging'?'继续充电':detail?.connected?'暂停充电':'已拔线';}
    else if(event.kind==='protocol_changed')desc=protocol(detail?.from)+' → '+protocol(detail?.to);
    else if(event.kind==='data_gap')desc=elapsed(detail?.from_elapsed_ms)+'—'+elapsed(detail?.to_elapsed_ms)+' 数据缺失';
    else if(event.kind==='session_end')copy.textContent=endReason(detail?.reason);
    if(desc)copy.append(el('small',desc));node.append(copy,time);nodes.push(node);}
  if(!nodes.length){const node=el('div',null,'stats-event');node.append(el('span','开始记录'),el('time',clock(summary.started_unix_ms)));nodes.push(node);if(summary.ended_unix_ms!=null){const end=el('div',null,'stats-event');end.append(el('span',endReason(summary.end_reason)),el('time',clock(summary.ended_unix_ms)));nodes.push(end);}}
  $('statistics-events').replaceChildren(...nodes);
}
function endReason(reason){return ({disconnected:'已拔线',disabled:'自动记录关闭',module_stopped:'模块停止',interrupted:'记录中断',recording_error:'记录异常',power_state_unknown:'充电状态未知'})[reason]||'记录结束';}
function timeBounds(){const summary=currentSummary(),last=currentData()?.points.at(-1);const end=Math.max(1000,Number(summary?.elapsed_ms)||0,Number(last?.elapsed_ms)||0);if(view==='detail')return [Math.min(windowStart,end),Math.max(windowStart+1000,Math.min(windowEnd??end,end))];return [period==='recent'?Math.max(0,end-900000):0,end];}
function visiblePoints(){const [start,end]=timeBounds();return (currentData()?.points||[]).filter(point=>point.elapsed_ms>=start&&point.elapsed_ms<=end);}
function valueRange(points,key){const values=points.map(point=>number(point[key])).filter(value=>value!=null);if(key==='soc')return [0,100];if(!values.length)return key==='temp_c'?[25,45]:[0,60];let low=Math.min(...values),high=Math.max(...values);if(key==='temp_c'){low=Math.floor((low-1)/5)*5;high=Math.ceil((high+1)/5)*5;}else{low=Math.min(0,low);const span=Math.max(1,high-low);high+=span*.1;const step=Math.pow(10,Math.floor(Math.log10(Math.max(1,high))))/2;high=Math.ceil(high/step)*step;}return low===high?[low,high+1]:[low,high];}
function pathFor(points,key,bounds,limits,box){
  const [start,end]=bounds,[low,high]=limits,{left,right,top,bottom}=box;let path='',previous=null;
  for(const point of points){const value=number(point[key]);if(value==null){previous=null;continue;}const x=left+(point.elapsed_ms-start)/Math.max(1,end-start)*(right-left),y=bottom-(value-low)/Math.max(1,high-low)*(bottom-top);const move=!previous||point.gap_before;path+=(move?'M':'L')+x.toFixed(2)+' '+y.toFixed(2);previous=point;}
  return path;
}
function renderChart(id,points,key,bounds,selected){
  const svg=$(id),box={left:43,right:541,top:12,bottom:133},limits=valueRange(points,key),[start,end]=bounds,summary=currentSummary();
  const [low,high]=limits;let markup='';
  for(let i=0;i<4;i++){const y=box.bottom-(box.bottom-box.top)*i/3,value=low+(high-low)*i/3;markup+='<line class="stats-grid" x1="43" x2="541" y1="'+y+'" y2="'+y+'"/><text class="stats-axis" text-anchor="end" x="34" y="'+(y+6)+'">'+format(value,key==='temp_c'?0:key==='net_power_w'&&high<10?1:0)+'</text>';}
  for(let i=0;i<5;i++){const x=box.left+(box.right-box.left)*i/4,t=start+(end-start)*i/4;markup+='<text class="stats-axis" text-anchor="'+(i===0?'start':i===4?'end':'middle')+'" x="'+x+'" y="161">'+elapsed(t)+'</text>';}
  let previous=null;for(const point of points){if(previous&&point.gap_before){const x1=box.left+(previous.elapsed_ms-start)/(end-start)*(box.right-box.left),x2=box.left+(point.elapsed_ms-start)/(end-start)*(box.right-box.left);markup+='<rect class="stats-gap" x="'+x1+'" y="12" width="'+Math.max(0,x2-x1)+'" height="121"/>';}previous=point;}
  const path=pathFor(points,key,bounds,limits,box);markup+='<path class="stats-line" d="'+path+'"/>';
  for(let i=0;i<points.length;i++){const point=points[i],value=number(point[key]),before=points[i-1],after=points[i+1];if(value!=null&&(!before||point.gap_before||number(before[key])==null)&&(!after||after.gap_before||number(after[key])==null)){const x=box.left+(point.elapsed_ms-start)/(end-start)*(box.right-box.left),y=box.bottom-(value-low)/(high-low)*(box.bottom-box.top);markup+='<circle class="stats-point" cx="'+x+'" cy="'+y+'" r="3"/>';}}
  if(!path)markup+='<text class="stats-missing" x="292" y="79" text-anchor="middle">暂无有效读数</text>';
  if(selected){const x=box.left+(selected.elapsed_ms-start)/(end-start)*(box.right-box.left);markup+='<line class="stats-cursor-line" x1="'+x+'" x2="'+x+'" y1="8" y2="136"/>';const value=number(selected[key]);if(value!=null){const y=box.bottom-(value-low)/(high-low)*(box.bottom-box.top);markup+='<circle class="stats-point" cx="'+x+'" cy="'+y+'" r="4.5"/>';}}
  svg.innerHTML=markup;
}
function renderCharts(){
  const data=currentData(),summary=currentSummary();if(!summary)return;
  const points=visiblePoints(),bounds=timeBounds(),[start,end]=bounds;
  const selected=cursorElapsed==null?null:points.reduce((best,point)=>!best||Math.abs(point.elapsed_ms-cursorElapsed)<Math.abs(best.elapsed_ms-cursorElapsed)?point:best,null);
  const info=metrics[metric];text('statistics-upper-label',info.label+' · '+info.unit);$('statistics-upper-chart').setAttribute('aria-label',info.label+'曲线');
  renderChart('statistics-upper-chart',points,metric,bounds,selected);renderChart('statistics-temperature-chart',points,'temp_c',bounds,selected);
  const tooltip=$('statistics-cursor');tooltip.hidden=!selected;
  if(selected){tooltip.textContent=clock(selected.unix_ms)+' · '+elapsed(selected.elapsed_ms)+'\n'+format(selected[metric],metric==='soc'?0:1)+' '+info.unit+' · '+format(selected.temp_c)+' ℃';tooltip.style.left=Math.max(19,Math.min(81,100*(43+(selected.elapsed_ms-start)/(end-start)*498)/560))+'%';}
  const timeline=$('statistics-protocol-track'),segments=[];let current=null;
  for(let i=0;i<points.length;i++){const point=points[i],name=point.protocol||'unknown',to=Math.min(end,points[i+1]?.elapsed_ms??end),from=Math.max(start,point.elapsed_ms);if(to<=from)continue;const gap=points[i+1]?.gap_before;
    if(current&&current.name===name&&!point.gap_before&&!current.gap&&!gap)current.to=to;else{current={name,from,to,gap};segments.push(current);}}
  timeline.replaceChildren(...segments.map(segment=>{const node=el('span',segment.gap?'':protocol(segment.name),segment.gap?'stats-protocol-gap':null);node.style.flex=String(Math.max(1,segment.to-segment.from));node.title=segment.gap?'数据缺失':protocol(segment.name);return node;}));
  if(!segments.length)timeline.append(el('span',data?'协议记录不可用':'正在读取曲线'));
  if(view==='detail'){
    const max=Math.max(1000,Number(summary.elapsed_ms)||0),to=windowEnd??max;
    for(const id of ['statistics-window-start','statistics-window-end'])$(id).max=String(max);
    $('statistics-window-start').value=String(windowStart);$('statistics-window-end').value=String(to);
    text('statistics-window-start-label',elapsed(windowStart));text('statistics-window-end-label',elapsed(to));$('statistics-window-selection').style.left=windowStart/max*100+'%';$('statistics-window-selection').style.width=(to-windowStart)/max*100+'%';
  }
  text('statistics-chart-note',!data?'正在读取曲线…':!points.length?'此时间范围暂无采样，读数不可用时显示 —':'按实际时间绘制，缺失处断开 · 滑动查看读数');
}
function sparkline(summary){
  const points=summary.sparkline||[],svg=document.createElementNS(ns,'svg');svg.setAttribute('viewBox','0 0 300 76');svg.setAttribute('class','stats-sparkline');svg.setAttribute('aria-hidden','true');
  const bounds=[0,Math.max(1,Number(summary.elapsed_ms)||0)],upper=pathFor(points,'net_power_w',bounds,valueRange(points,'net_power_w'),{left:1,right:299,top:2,bottom:39}),lower=pathFor(points,'temp_c',bounds,valueRange(points,'temp_c'),{left:1,right:299,top:49,bottom:73});
  svg.innerHTML='<path class="stats-spark-grid" d="M0 40H300M0 75H300"/><path class="stats-spark-power" d="'+upper+'"/><path class="stats-spark-temp" d="'+lower+'"/>';return svg;
}
function renderHistory(){
  text('statistics-total',listLoaded?(aggregate.count??total):'—');text('statistics-charging-time',listLoaded?duration(aggregate.charging_ms):'—');text('statistics-history-peak',listLoaded?format(aggregate.power_peak_w)+' W':'—');text('statistics-filter-label',filterLabel());groupButtons('[data-stat-range]',range);
  $('statistics-export-filtered').disabled=exportStarting||!CG.available()||!listLoaded||!total;
  const nodes=[];let lastDay='';
  for(const summary of items){const day=dateInput(summary.started_unix_ms);if(day!==lastDay){const heading=el('h3',day===dateInput(Date.now())?'今天':day===dateInput(Date.now()-86400000)?'昨天':date(summary.started_unix_ms),'stats-date-heading');nodes.push(heading);lastDay=day;}
    const button=el('button',null,'stats-history-record');button.type='button';button.setAttribute('aria-label',date(summary.started_unix_ms)+clock(summary.started_unix_ms)+'，'+soc(summary.start_soc)+'至'+soc(summary.end_soc)+'，查看充电详情');button.onclick=()=>openDetail(summary.id);
    const heading=el('div',null,'stats-record-heading'),badge=el('span',protocols(summary),'charge-badge protocol-badge'),time=el('time','· '+clock(summary.started_unix_ms)),socText=el('span',soc(summary.start_soc)+' → '+soc(summary.end_soc),'stats-record-soc');heading.append(badge,time,socText,el('span',duration(summary.charging_ms)),svgIcon('next'));
    const chart=el('div',null,'stats-record-body'),values=el('div',null,'stats-record-metrics'),peak=el('div','峰值 '),temp=el('div','最高 ');peak.append(el('strong',format(summary.power_peak_w)+' W'));temp.append(el('strong',format(summary.temp_max_c)+' ℃','stats-record-temp'));values.append(peak,temp);chart.append(sparkline(summary),values);button.append(heading,chart);if(summary.ended_unix_ms==null)button.append(el('p',sessionState(summary).text,'stats-record-status'));else if(!summary.complete)button.append(el('p','部分记录','stats-record-status'));nodes.push(button);
  }
  if(!nodes.length){const empty=el('div',null,'card stats-empty stats-empty-history');empty.append(svgIcon('statistics'),el('h2',!listLoaded?'正在读取记录':total?'正在加载记录':filterLabel()!=='筛选'||range==='custom'?'筛选无结果':'暂无充电记录'),el('p',!listLoaded?'请稍候':'没有符合当前条件的记录','hint'));if(listLoaded){const reset=el('button','重置筛选','stats-outline');reset.type='button';reset.onclick=resetFilter;empty.append(reset);}nodes.push(empty);}
  $('statistics-history-list').replaceChildren(...nodes);text('statistics-history-count',listLoaded?'已加载 '+items.length+' / '+total+' 条':'');$('statistics-load-more').hidden=items.length>=total||!listLoaded;
}
function openDetail(id){selectedId=id;const summary=items.find(item=>item.id===id);selectedDetail=summary?{summary,points:[],events:[],cursor:0}:null;setView('detail');}
function resetFilter(){range='7';filter={protocols:[],completeness:'all'};filterGeneration++;items=[];listLoaded=false;renderHistory();loadList();}
function outside(event){const box=dialog.getBoundingClientRect();return event.target===dialog&&(event.clientX<box.left||event.clientX>box.right||event.clientY<box.top||event.clientY>box.bottom);}
function openDialog(kind,title,wide=false){dialogKind=kind;dialog.classList.toggle('stats-filter-dialog',wide);text('statistics-dialog-title',title);body.replaceChildren();if(!dialog.open){history.pushState({...history.state,chargeGuardStatisticsDialog:true},'');dialogHistory=true;dialog.showModal();}return body;}
function closeDialog(){dialog.close();}
function actionButton(label,cls,callback){const button=el('button',label,cls);button.type='button';button.onclick=callback;return button;}
function option(value,label){const node=el('option',label);node.value=String(value);return node;}
function openFilter(custom=false){
  const content=openDialog('filter','筛选记录',true),draft={...selectedFilter(),protocols:[...filter.protocols]},draftRange={value:custom?'custom':range};
  content.append(el('h3','时间范围'));
  const ranges=el('div',null,'stats-segments'),dates=el('div',null,'stats-filter-dates'),from=document.createElement('input'),to=document.createElement('input');from.type=to.type='date';from.value=dateInput(draft.from_ms??Date.now()-6*86400000);to.value=dateInput(draft.to_ms??Date.now());from.setAttribute('aria-label','开始日期');to.setAttribute('aria-label','结束日期');from.max=to.max=dateInput(Date.now());dates.append(from,to);
  const rangeButtons=[];for(const value of ['7','30','90','custom']){const button=actionButton(value==='custom'?'自定义':'近'+value+'天','',()=>{draftRange.value=value;paintRanges();});rangeButtons.push(button);ranges.append(button);}
  function paintRanges(){rangeButtons.forEach((button,i)=>button.setAttribute('aria-pressed',String(['7','30','90','custom'][i]===draftRange.value)));dates.hidden=draftRange.value!=='custom';}paintRanges();content.append(ranges,dates,el('h3','充电协议'));
  const checks=el('div',null,'protocol-grid');for(const [value,label] of [['supervooc','SuperVOOC'],['ufcs','UFCS'],['pps','PPS'],['pd','PD'],['other','其他']]){const choice=el('label',null,'protocol-choice'),input=document.createElement('input'),box=el('span',null,'protocol-option');input.type='checkbox';input.value=value;input.checked=draft.protocols.includes(value);box.append(el('span',label),el('span',null,'protocol-check'));choice.append(input,box);checks.append(choice);}content.append(checks,el('p','包含任一所选协议；不选则显示全部','hint'),el('h3','电量区间'));
  const socRow=el('div',null,'stats-filter-soc'),startLabel=el('label','起始电量'),endLabel=el('label','结束电量'),start=document.createElement('select'),end=document.createElement('select');
  start.append(option('','不限'));end.append(option('','不限'));for(let i=10;i<=100;i+=10)start.append(option(i,'≤ '+i+'%'));for(let i=0;i<=100;i+=10)end.append(option(i,'≥ '+i+'%'));start.value=filter.start_soc_max??'';end.value=filter.end_soc_min??'';startLabel.append(start);endLabel.append(end);socRow.append(startLabel,endLabel);content.append(socRow,el('h3','记录完整性'));
  const completeness=el('div',null,'stats-segments');let selectedCompleteness=filter.completeness;const completeButtons=[];for(const [value,label] of [['all','全部'],['complete','完整'],['partial','部分']]){const button=actionButton(label,'',()=>{selectedCompleteness=value;paintComplete();});button.dataset.value=value;completeButtons.push(button);completeness.append(button);}function paintComplete(){for(const button of completeButtons)button.setAttribute('aria-pressed',String(button.dataset.value===selectedCompleteness));}paintComplete();content.append(completeness);
  const error=el('p','','hint error-text');error.setAttribute('role','status');content.append(error);const actions=el('div',null,'stats-dialog-actions');actions.append(actionButton('重置','secondary',()=>{closeDialog();resetFilter();}),actionButton('应用筛选','primary',()=>{
    const next={protocols:Array.from(checks.querySelectorAll('input:checked'),node=>node.value),completeness:selectedCompleteness};
    if(start.value!=='')next.start_soc_max=Number(start.value);if(end.value!=='')next.end_soc_min=Number(end.value);
    if(draftRange.value==='custom'){if(!from.value||!to.value||from.value>to.value){error.textContent='请选择有效的开始和结束日期';return;}next.from_ms=new Date(from.value+'T00:00:00').getTime();next.to_ms=new Date(to.value+'T23:59:59.999').getTime();}
    range=draftRange.value;filter=next;filterGeneration++;items=[];listLoaded=false;closeDialog();renderHistory();loadList();
  }));content.append(actions);
}
function exportScope(kind){if(kind==='session'){const summary=currentSummary();return {ids:[summary.id],count:1,label:'单次充电记录',description:date(summary.started_unix_ms)+' '+clock(summary.started_unix_ms)};}if(kind==='all')return {filter:{protocols:[],completeness:'all'},count:storage?.sessions??total,label:'全部记录',description:'包含当前正在记录的会话快照'};const selected=selectedFilter();return {filter:selected,count:total,label:'当前筛选',description:date(selected.from_ms)+'—'+date(selected.to_ms)};}
function openExport(kind){
  if(job&&['queued','running'].includes(job.state)){showExportJob();return;}
  const scope=exportScope(kind),content=openDialog('export-select','导出数据集',true),scopeNode=el('div',scope.label+' · '+scope.count+'条记录','stats-export-scope');scopeNode.append(el('p',scope.description,'hint'));content.append(scopeNode,el('h3','数据内容'));
  const contents=el('div',null,'stats-export-contents');for(const [title,description] of [['会话汇总','起止电量、时长、峰值与均值'],['原始采样','真实时间、功率、电流、电压、温度'],['充电事件','协议变化、暂停、满电与中断'],['运行上下文','系统版本、模块版本、充电设置']]){const row=el('div',null,'stats-export-content'),copy=el('div');copy.append(el('strong',title),el('small',description));row.append(svgIcon('statistics'),copy);contents.append(row);}content.append(contents,el('p','CSV + JSON · ZIP，保存到 Download','hint'),el('p','保留完整采样，不压缩曲线精度。导出开始后固定范围。','hint'));
  const actions=el('div',null,'stats-dialog-actions'),button=actionButton('导出'+scope.count+'条记录','primary',()=>startExport(scope));button.disabled=exportStarting||scope.count===0;actions.append(button);content.append(actions);
}
async function startExport(scope){
  if(exportStarting)return;exportStarting=true;job={state:'queued',processed:0,total:scope.count,label:scope.label};showExportJob();renderStorage();renderSession();renderHistory();
  try{const result=await request({action:'export-start',...(scope.ids?{ids:scope.ids}:{filter:scope.filter})});job={...job,...result,state:result.state||'queued'};if(!job.job_id)throw Error('未收到导出任务标识');if(live())await pollExport();schedule(1000);}
  catch(error){job={...job,state:'failed',error:error.message};if(dialog.open&&dialogKind==='export-job')showExportJob(false);else if(opened)showJobNotice();}
  finally{exportStarting=false;renderStorage();renderSession();renderHistory();}
}
async function pollExport(){
  if(!live()||exportPolling||!job?.job_id||!['queued','running'].includes(job.state))return;
  const id=job.job_id;exportPolling=true;
  try{const result=await request({action:'export-status',job_id:id});if(job?.job_id!==id)return;job={...job,...result};if(dialog.open&&dialogKind==='export-job')showExportJob(false);else if(opened)showJobNotice();}
  catch(error){if(job?.job_id===id&&opened)notice('导出进度暂不可用：'+error.message+'；任务可能仍在后台继续',true);}
  finally{exportPolling=false;}
}
function showJobNotice(){
  const node=$('statistics-notice');node.hidden=false;node.classList.toggle('error',job.state==='failed');node.replaceChildren();const label=job.state==='complete'?'数据集已导出':job.state==='failed'?'导出失败':'正在导出 '+(job.processed||0)+' / '+(job.total||0)+' 条';const row=el('div',null,'stats-job-banner');row.append(el('span',label),actionButton('查看','stats-outline',()=>showExportJob()));node.append(row);
}
function showExportJob(open=true){
  if(!job)return;const running=['queued','running'].includes(job.state),success=job.state==='complete';
  const content=open?openDialog('export-job',running?'正在导出':success?'数据集已导出':'导出失败'):body;
  if(!open){text('statistics-dialog-title',running?'正在导出':success?'数据集已导出':'导出失败');content.replaceChildren();}
  const wrap=el('div',null,'stats-export-result'),icon=el('span',null,'export-dialog-icon');icon.append(svgIcon(success?'download':job.state==='failed'?'info':'download'));wrap.append(icon,el('h3',running?'正在导出':success?'数据集已导出':'导出失败'));
  wrap.append(el('p',(job.label||'充电记录')+' · '+(job.total||0)+'条记录'+(success&&job.bytes?' · '+bytes(job.bytes):''),'hint'));
  if(running){const phase=({samples:'原始采样',events:'充电事件',context:'运行上下文',publish:'保存文件'})[job.phase]||'准备数据';wrap.append(el('p',phase+' · 已整理 '+(job.processed||0)+' / '+(job.total||0)+' 条','stats-export-progress-label'));const progress=el('progress',null,'stats-progress');progress.max=Math.max(1,job.total||0);progress.value=job.processed||0;progress.setAttribute('aria-label','数据集导出进度');wrap.append(progress,el('p','可关闭弹窗，继续使用','hint'));}
  else if(success){const path=job.public_path||job.path;if(path){const box=el('div',null,'export-path-box'),pathNode=el('div',path,'stats-export-path');pathNode.tabIndex=0;box.append(el('p','保存位置'),pathNode);wrap.append(box);const status=el('p','','export-copy-status hint');status.setAttribute('role','status');const button=actionButton('复制路径','secondary stats-wide',()=>copyPath(path,status,button));wrap.append(button,status,el('p',job.public_path?'已保存到下载目录':'已保留内部副本，访问此位置需要 Root 权限。','hint'));if(job.public_error)wrap.append(el('p','下载目录保存失败：'+job.public_error,'hint error-text'));}else wrap.append(el('p','未收到有效保存路径，请检查模块日志','hint error-text'));}
  else wrap.append(el('p',job.error||'请稍后重试','hint error-text'));
  content.append(wrap);
}
async function copyPath(path,status,button){
  button.disabled=true;let copied=false;
  try{if(navigator.clipboard?.writeText){await navigator.clipboard.writeText(path);copied=true;}}catch(_){}
  if(!copied){const field=el('textarea');field.value=path;field.readOnly=true;field.className='clipboard-buffer';dialog.append(field);field.select();try{copied=document.execCommand('copy');}catch(_){}field.remove();}
  status.textContent=copied?'路径已复制':'复制未成功，请长按上方路径手动复制。';button.disabled=false;if(dialog.open)button.focus({preventScroll:true});
}
function confirmDelete(all=false){
  const summary=currentSummary();if(!all&&(!summary||summary.ended_unix_ms==null))return;
  const count=all?Math.max(0,(storage?.sessions||0)-(storage?.active||0)):1,content=openDialog('delete',all?'清理历史记录？':'删除这条记录？'),copy=el('div',null,'stats-delete-copy');const icon=svgIcon('trash');icon.classList.add('stats-delete-icon');copy.append(icon,el('p',all?'将删除'+count+'条历史记录。':'将删除这次充电的汇总、采样与事件。'),el('p',all?'正在记录的本次充电会保留。':'删除后无法恢复。','hint'));content.append(copy);
  const error=el('p','','hint error-text'),actions=el('div',null,'stats-dialog-actions');const remove=actionButton(all?'清理':'删除','stop',async()=>{
    if(deleting)return;deleting=true;remove.disabled=true;
    try{await request(all?{action:'delete',all:true}:{action:'delete',ids:[summary.id]});const returnToList=!all&&view==='detail'&&selectedId===summary.id;
      if(dialog.open&&dialogKind==='delete'){returnAfterDialog=returnToList;closeDialog();}else if(returnToList)leaveSubview();
      items=[];listLoaded=false;storage=null;if(all||selectedId===summary.id)selectedDetail=null;
      if(all||latestSession?.id===summary.id)latestSession=null;if(all&&!active||!all&&activeDetail?.summary?.id===summary.id)activeDetail=null;
      notice(all?'历史记录已清理':'记录已删除');if(live()){await loadList();if(view==='settings')await loadStorage();}}
    catch(failure){error.textContent='删除失败：'+failure.message;}
    finally{deleting=false;remove.disabled=false;renderStorage();renderSession();}
  });actions.append(actionButton('取消','secondary',closeDialog),remove);content.append(error,actions);
}
function openMeasurement(){
  const content=openDialog('measurement','数据口径'),copy=el('div',null,'stats-measurement-copy');
  copy.append(el('p','功率采用电池端净功率，反映进入电池的能量变化，不等同于充电器或 USB 输入功率。系统自身耗电会影响读数。'));
  const source=view==='settings'?measurement:currentSummary()?.measurement||measurement;
  copy.append(el('p','电流正值表示充入电池，电压为代表电芯电压。仅在设备电池口径与芯数均已确认时，以电流×单芯电压×芯数计算电池净功率；无法确认或读取的数值显示 —。原始读数、单位及换算信息随数据集导出。'));
  copy.append(el('p','每5秒采样；充满后停止周期采样。页面关闭不影响模块记录，停止模块或关闭自动记录会中止采样。系统时间变化、重启与采样中断可能产生部分记录，缺失区间不连线。'));
  copy.append(el('p','曲线使用实际采样时间，历史缩略图与全程曲线会适度抽稀；导出的原始采样保留完整记录。'));
  if(source){copy.append(el('p','当前记录口径：'+(source.cell_count?source.cell_count+'芯':'芯数未确认')+'；'+(source.power_available?'电池净功率可用':'电池净功率不可用')+'。'));if(typeof source.note==='string')copy.append(el('p',source.note));}
  content.append(copy);
}
function chooseCursor(event){
  const points=visiblePoints();if(!points.length)return;
  const box=$('statistics-upper-chart').getBoundingClientRect(),x=(event.clientX-box.left)/box.width,[start,end]=timeBounds();cursorElapsed=start+Math.max(0,Math.min(1,(x*560-43)/498))*(end-start);renderCharts();
}
const touch=$('statistics-chart-touch');let chartPointer=null;
touch.addEventListener('pointerdown',event=>{if(event.button!==0)return;chartPointer=event.pointerId;touch.setPointerCapture(event.pointerId);chooseCursor(event);});
touch.addEventListener('pointermove',event=>{if(chartPointer===event.pointerId)chooseCursor(event);});
for(const name of ['pointerup','pointercancel','lostpointercapture'])touch.addEventListener(name,()=>{chartPointer=null;});
touch.addEventListener('keydown',event=>{if(!['ArrowLeft','ArrowRight','Escape'].includes(event.key))return;event.preventDefault();if(event.key==='Escape'){cursorElapsed=null;renderCharts();return;}const points=visiblePoints();if(!points.length)return;let index=cursorElapsed==null?0:points.reduce((best,point,i)=>Math.abs(point.elapsed_ms-cursorElapsed)<Math.abs(points[best].elapsed_ms-cursorElapsed)?i:best,0);index=Math.max(0,Math.min(points.length-1,index+(event.key==='ArrowRight'?1:-1)));cursorElapsed=points[index].elapsed_ms;renderCharts();});
for(const input of [$('statistics-window-start'),$('statistics-window-end')])input.addEventListener('input',()=>{
  const max=Number(input.max),minimum=Math.min(1000,max);let start=Number($('statistics-window-start').value),end=Number($('statistics-window-end').value);
  if(input.id==='statistics-window-start')start=Math.max(0,Math.min(start,end-minimum));else end=Math.min(max,Math.max(end,start+minimum));
  windowStart=start;windowEnd=end;windowGeneration++;cursorElapsed=null;renderCharts();if(windowTimer!==null)clearTimeout(windowTimer);windowTimer=setTimeout(()=>{windowTimer=null;if(live()&&view==='detail')loadDetail(true,true);},250);
});
for(const button of document.querySelectorAll('[data-stat-metric]'))button.onclick=()=>{metric=button.dataset.statMetric;groupButtons('[data-stat-metric]',metric);renderCharts();};
for(const button of document.querySelectorAll('[data-stat-period]'))button.onclick=()=>{period=button.dataset.statPeriod;cursorElapsed=null;groupButtons('[data-stat-period]',period);renderCharts();};
for(const button of document.querySelectorAll('[data-stat-range]'))button.onclick=()=>{if(button.dataset.statRange==='custom'){openFilter(true);return;}range=button.dataset.statRange;filterGeneration++;items=[];listLoaded=false;renderHistory();loadList();};
for(const key of ['current','history'])$('statistics-tab-'+key).onclick=()=>setView(key);
for(const key of ['current','history'])$('statistics-tab-'+key).addEventListener('keydown',event=>{if(['ArrowLeft','ArrowRight','Home','End'].includes(event.key)){event.preventDefault();const next=event.key==='Home'?'current':event.key==='End'?'history':key==='current'?'history':'current';setView(next);$('statistics-tab-'+next).focus();}});
$('statistics-settings').onclick=$('statistics-empty-settings').onclick=()=>setView('settings');$('statistics-back').onclick=leaveSubview;
$('statistics-filter').onclick=()=>openFilter();$('statistics-load-more').onclick=()=>loadList(true);
$('statistics-export-filtered').onclick=()=>openExport('filtered');$('statistics-export-session').onclick=()=>openExport('session');$('statistics-export-all').onclick=()=>openExport('all');
$('statistics-clear').onclick=()=>confirmDelete(true);$('statistics-delete-session').onclick=()=>confirmDelete(false);
$('statistics-measurement').onclick=$('statistics-settings-measurement').onclick=openMeasurement;
$('statistics-dialog-close').onclick=closeDialog;
dialog.addEventListener('pointerdown',event=>{backdropPress=outside(event);});dialog.addEventListener('pointercancel',()=>{backdropPress=false;});dialog.addEventListener('click',event=>{if(backdropPress&&outside(event))dialog.close();backdropPress=false;});
dialog.addEventListener('cancel',event=>{event.preventDefault();dialog.close();});
dialog.addEventListener('close',()=>{backdropPress=false;dialogKind='';if(dialogHistory){dialogHistory=false;if(history.state?.chargeGuardStatisticsDialog)history.back();}});
window.addEventListener('popstate',()=>{
  if(dialogHistory){dialogHistory=false;if(dialog.open)dialog.close();return;}
  if(returnAfterDialog&&!history.state?.chargeGuardStatisticsDialog){returnAfterDialog=false;leaveSubview();return;}
  if(viewHistory&&!history.state?.chargeGuardStatisticsView){viewHistory=false;setView(returnView,false);}
});
window.CGStatistics={
  controls({page}){$('core-badge').hidden=page==='statistics';header();},
  config(next){enabled=next.charge_statistics_enabled!==false;if(opened)renderSession();},
  status(data){runtime=data;recordingError=data.status?.statistics_error||null;if(opened)renderSession();},
  page(name){const was=opened;opened=name==='statistics';header();if(!opened){stop();return;}present();if(!was){loadList();if(view==='settings')loadStorage();else if(view==='detail')loadDetail();if(job&&['queued','running'].includes(job.state))pollExport();schedule();}},
  visibility(open){viewOpen=open;if(!live()){stop();return;}if(view==='current'||view==='history')loadList();if(view==='settings')loadStorage();else if(view==='detail')loadDetail();if(job&&['queued','running'].includes(job.state))pollExport();schedule();}
};
})();
