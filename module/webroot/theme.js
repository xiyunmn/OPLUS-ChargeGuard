/* Local preference only. No backend configuration changes. */
(function(){'use strict';
let scale=105;try{const v=Number(localStorage.getItem('chargeguard-ui-scale'));if([95,100,105,110,115].includes(v))scale=v;}catch(_){}
function applyScale(){document.documentElement.dataset.uiScale=String(scale);}
window.CGDisplay={get:()=>scale,set:v=>{if(![95,100,105,110,115].includes(v))return;scale=v;try{localStorage.setItem('chargeguard-ui-scale',String(v));}catch(_){}applyScale();}};
applyScale();
const media=matchMedia('(prefers-color-scheme: dark)');
let mode='system';try{const v=localStorage.getItem('chargeguard-theme');if(['light','dark','system'].includes(v))mode=v;}catch(_){}
function apply(){const dark=mode==='dark'||(mode==='system'&&media.matches);document.documentElement.dataset.theme=dark?'dark':'light';document.getElementById('theme-color').content=dark?'#0d1526':'#f3f6fc';}
window.CGTheme={get:()=>mode,set:v=>{if(!['light','dark','system'].includes(v))return;mode=v;try{localStorage.setItem('chargeguard-theme',v);}catch(_){}apply();}};
media.addEventListener('change',apply);apply();
})();
