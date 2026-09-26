/* Local appearance preferences, applied before the first page is shown. */
(function(){'use strict';
const root=document.documentElement,scaleKey='chargeguard-ui-scale-v2';
const scales=[85,90,95,100,105,110,115];
let scale=100;
try{
  const stored=localStorage.getItem(scaleKey),value=Number(stored);
  if(stored!==null&&scales.includes(value))scale=value;
  else if(stored===null){
    const legacy=Number(localStorage.getItem('chargeguard-ui-scale'));
    if([95,100,105,110,115].includes(legacy))scale=Math.max(85,Math.min(115,Math.round(legacy/115*100/5)*5));
  }
  localStorage.setItem(scaleKey,String(scale));
}catch(_){}
function applyScale(){root.dataset.uiScale=String(scale);}
window.CGDisplay={get:()=>scale,min:85,max:115,default:100,set:value=>{
  if(!scales.includes(value))return;scale=value;
  try{localStorage.setItem(scaleKey,String(value));}catch(_){}applyScale();
}};
applyScale();
const media=matchMedia('(prefers-color-scheme: dark)');
let mode='system';try{const value=localStorage.getItem('chargeguard-theme');if(['light','dark','system'].includes(value))mode=value;}catch(_){}
function apply(){const dark=mode==='dark'||(mode==='system'&&media.matches);root.dataset.theme=dark?'dark':'light';document.getElementById('theme-color').content=dark?'#0d1526':'#f3f6fc';}
window.CGTheme={get:()=>mode,set:value=>{if(!['light','dark','system'].includes(value))return;mode=value;try{localStorage.setItem('chargeguard-theme',value);}catch(_){}apply();}};
media.addEventListener('change',apply);apply();

root.dataset.startup='pending';
let insetsReady=null,revealing=null;
function prepare(){
  if(insetsReady)return insetsReady;
  if(!window.ksu||typeof window.ksu.exec!=='function')return Promise.resolve();
  insetsReady=new Promise(resolve=>{
    const link=document.createElement('link');let done=false;
    const finish=loaded=>{
      if(done)return;done=true;clearTimeout(timer);link.onload=null;link.onerror=null;
      if(!loaded){link.disabled=true;link.remove();}resolve();
    };
    const timer=setTimeout(()=>finish(false),1500);
    link.rel='stylesheet';link.href='/internal/insets.css';link.setAttribute('blocking','render');
    link.onload=()=>finish(true);link.onerror=()=>finish(false);document.head.append(link);
  });
  return insetsReady;
}
function reveal(){
  if(revealing)return revealing;
  clearTimeout(fallback);
  revealing=prepare().then(()=>new Promise(resolve=>requestAnimationFrame(()=>{
    // Commit final sizes and checked states with startup transitions disabled.
    if(document.body)document.body.getBoundingClientRect();
    root.dataset.startup='ready';resolve();
  })));
  return revealing;
}
const fallback=setTimeout(reveal,7000);
window.CGStartup={prepare,reveal};prepare();
})();
