(() => {
  const $=s=>document.querySelector(s);
  const defaults={view:'network',word:'p1',family:'synapse',identity:true,easterEggs:true,morph:true,agents:true,threads:true,halo:true,pulse:false,gather:true,neurons:true,connections:true,signals:true,motion:true,heading:true,prompt:true,tribute:'love',strength:1,speed:1,logoScale:1};
  const choices={view:['network','logo'],word:['p1','phaseone','alternate'],family:['stepped','contour','synapse'],tribute:['love','pie','portal','inspired','off']};
  function normalise(next){
    const out={...defaults};
    for(const key of Object.keys(defaults))if(typeof next?.[key]===typeof defaults[key])out[key]=next[key];
    for(const [key,values] of Object.entries(choices))if(!values.includes(out[key]))out[key]=defaults[key];
    for(const [key,low,high] of [['strength',.4,2],['speed',.25,2],['logoScale',.65,1.4]])out[key]=Number.isFinite(out[key])?Math.max(low,Math.min(high,out[key])):defaults[key];
    return out;
  }
  let settings={...defaults},scene='awakening';
  const mask=document.createElement('canvas');mask.width=1000;mask.height=660;
  const ctx=mask.getContext('2d',{willReadFrequently:true});
  let maskData=null,maskKey='',maskVersion=0,loadedVersion=0,activeWord='p1',targets=[];
  function refreshMask(){
    const key=settings.family+activeWord+settings.logoScale;if(key===maskKey)return;maskKey=key;
    const version=++maskVersion,img=new Image();
    img.onload=()=>{if(version!==maskVersion)return;ctx.clearRect(0,0,1000,660);const m=phaseoneLogos.mark(settings.family,activeWord),width=(activeWord==='p1'?242:342)*settings.logoScale,height=width*m.height/m.width;ctx.drawImage(img,500-width/2,326-height/2,width,height);maskData=ctx.getImageData(0,0,1000,660).data;targets=[];for(let y=100;y<550;y+=3.4)for(let x=200;x<800;x+=3.4)if(maskData[(Math.round(y)*1000+Math.round(x))*4+3]>150)targets.push({x:x+Math.sin(x*17+y*31)*.85,y:y+Math.cos(x*13+y*19)*.85});if(targets.length>1100){const step=targets.length/1100;targets=Array.from({length:1100},(_,i)=>targets[Math.floor(i*step)]);}loadedVersion=version;document.dispatchEvent(new Event('maskready'));};
    img.src='data:image/svg+xml;charset=utf-8,'+encodeURIComponent(phaseoneLogos.svg(settings.family,activeWord));
  }
  function update(){
    for(const key of Object.keys(defaults)){const el=$(`[data-setting="${key}"]`);if(el){if(el.type==='checkbox')el.checked=settings[key];else el.value=settings[key];}}
    $('.intro').hidden=!settings.heading;$('.prompt').hidden=!settings.prompt;
    const tribute=$('#tribute');tribute.hidden=settings.tribute==='off';
    tribute.textContent={love:'we love Pi',pie:'we love 🥧',portal:'there is no pie',inspired:'inspired by Pi · made for phaseone'}[settings.tribute]||'';
    $('#identity-preview').innerHTML=phaseoneLogos.svg(scene==='awakening'?'synapse':settings.family,scene==='awakening'?'phaseone':settings.word==='alternate'?'p1':settings.word);
    refreshMask();document.dispatchEvent(new Event('settingschange'));
  }
  const params=new URLSearchParams(location.search);
  try{settings=normalise(JSON.parse(params.get('settings')||'null'));}catch{}
  window.playground={setScene(value){scene=value;update();},get ready(){return loadedVersion===maskVersion&&maskData!==null;},get mask(){return mask;},get targets(){return targets;},get maskVersion(){return maskVersion;},get settings(){return {...settings};},setSettings(next){settings=normalise(next);activeWord=settings.word==='alternate'?'p1':settings.word;maskKey='';update();},setActiveWord(cycle){const word=settings.word==='alternate'?(cycle%2?'phaseone':'p1'):settings.word;if(word!==activeWord){activeWord=word;refreshMask();}},contains(x,y){const ix=Math.round(x),iy=Math.round(y);return ix>=0&&ix<1000&&iy>=0&&iy<660&&maskData&&maskData[(iy*1000+ix)*4+3]>100;}};
  document.querySelectorAll('[data-setting]').forEach(el=>el.addEventListener('input',()=>{settings[el.dataset.setting]=el.type==='checkbox'?el.checked:el.type==='range'?+el.value:el.value;activeWord=settings.word==='alternate'?'p1':settings.word;update();}));
  const descriptions={stepped:'Pi homage / modular steps, reversed height hierarchy',contour:'Contour / open geometric strokes, rising numeral',synapse:'Selected / Synapse · connected dots'};
  for(const family of ['synapse','stepped','contour']){
    const card=document.createElement('article');card.className='logo-card';
    const title=document.createElement('h3');title.textContent=descriptions[family];card.append(title);
    for(const word of ['p1','phaseone']){
      const block=document.createElement('div');block.className='logo-option';block.innerHTML=phaseoneLogos.svg(family,word);
      const actions=document.createElement('div'),use=document.createElement('button'),download=document.createElement('a');
      use.textContent=`Inspect ${word}`;use.onclick=()=>{settings.view='logo';settings.family=family;settings.word=word;activeWord=word;update();neural.pause();neural.draw(neural.variant==='awakening'?0:6.6);$('#art').scrollIntoView({behavior:'smooth',block:'center'});};
      download.textContent='SVG ↓';download.href=`logos/${family}-${word}.svg`;download.download=`phaseone-${family}-${word}.svg`;const animate=document.createElement('button');animate.textContent='Animate';animate.onclick=()=>{settings.view='network';settings.family=family;settings.word=word;activeWord=word;update();neural.pause();neural.draw(neural.variant==='awakening'?0:6.6);$('#art').scrollIntoView({behavior:'smooth',block:'center'});};actions.append(use,animate,download);block.append(actions);card.append(block);
    }
    $('#logo-sheet').append(card);
  }
  document.querySelectorAll('[data-egg]').forEach(button=>button.onclick=()=>{neural.pause();neural.setVariant('awakening');window.playground.setSettings({...settings,view:'network',agents:true,easterEggs:true,motion:true});neural.setMode('fine');neural.draw(+button.dataset.egg);$('#art').scrollIntoView({behavior:'smooth',block:'center'});});
  $('#reset').onclick=()=>{activeWord='p1';window.playground.setSettings(defaults);};
  $('#share').onclick=()=>{const url=new URL(location.href);url.searchParams.set('variant',neural.variant);url.searchParams.set('t',neural.time.toFixed(2));url.searchParams.set('cycle',neural.cycle);url.searchParams.set('mode',neural.mode);url.searchParams.set('settings',JSON.stringify(settings));$('#share-url').hidden=false;$('#share-url').value=url.href;$('#share-url').select();};
  const desktop=matchMedia('(min-width:851px)'),controls=$('.playground');
  function placeControls(){if(desktop.matches)$('aside').prepend(controls);else $('.stage').before(controls);}
  desktop.addEventListener('change',placeControls);placeControls();
  update();
})();
