(() => {
  'use strict';
  const $=s=>document.querySelector(s),art=$('#art'),form=$('#composer');
  let pending=null,comments=[];
  function compose(x,y){
    neural.pause();pending={x,y,time:neural.time,mode:neural.mode,variant:neural.variant,revision:4,cycle:neural.cycle,settings:playground.settings,screenshot:$('#brain').toDataURL('image/png')};
    form.querySelector('label').textContent=`Feedback · ${neural.variant} · ${neural.time.toFixed(2)} s`;
    form.hidden=false;form.style.left=Math.min(x*art.clientWidth,Math.max(0,art.clientWidth-300))+'px';
    form.style.top=Math.min(y*art.clientHeight,Math.max(0,art.clientHeight-205))+'px';$('#note').focus();
  }
  $('#brain').onclick=e=>{const r=art.getBoundingClientRect();compose((e.clientX-r.left)/r.width,(e.clientY-r.top)/r.height);};
  $('#add').onclick=()=>compose(.5,.5);
  $('#cancel').onclick=()=>{form.hidden=true;pending=null;$('#note').value='';};
  document.addEventListener('keydown',e=>{if(e.key==='Escape')$('#cancel').click();});
  function show(c){neural.pause();neural.setCycle(c.cycle||0);if(c.settings)playground.setSettings(c.settings);neural.setVariant(c.variant||'original');neural.setMode(c.mode);neural.draw(c.time);}
  function render(){
    $('#count').textContent='/ '+comments.length;$('#comments').replaceChildren();$('#pins').replaceChildren();
    for(const [i,c] of comments.entries()){
      const li=document.createElement('li'),jump=document.createElement('button'),p=document.createElement('p');
      jump.textContent=`${i+1} · ${c.variant||'original'} · ${c.settings?.word||'p1'} · ${c.time.toFixed(2)} s`;jump.onclick=()=>show(c);
      p.textContent=c.text;const detail=document.createElement('small');detail.textContent=`${c.settings?.family||'original'} · ${c.settings?.view||'network'} · ${c.mode}`;li.append(jump,document.createElement('br'),detail,p);$('#comments').append(li);
      if((c.variant||'original')!==neural.variant)continue;
      const pin=document.createElement('button');pin.className='pin';pin.textContent=i+1;pin.title=c.text;
      pin.setAttribute('aria-label',`Comment ${i+1}: ${c.text}`);pin.style.left=c.x*100+'%';pin.style.top=c.y*100+'%';
      pin.onclick=()=>{show(c);li.scrollIntoView({behavior:'smooth',block:'nearest'});};$('#pins').append(pin);
    }
  }
  document.addEventListener('variantchange',render);
  form.onsubmit=async e=>{
    e.preventDefault();if(!pending||!$('#note').value.trim())return;
    const button=form.querySelector('[type=submit]');button.disabled=true;
    try{
      const response=await fetch('/api/comments',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({...pending,text:$('#note').value.trim()})});
      if(!response.ok)throw new Error('Save failed');
      comments.push(await response.json());render();form.hidden=true;pending=null;$('#note').value='';
      $('#status').textContent='Saved to workspace. The agent can read this note and frame.';
    }catch{$('#status').textContent='Could not save. Your note is still here; check that the preview server is running.';}
    finally{button.disabled=false;}
  };
  async function load(){try{const r=await fetch('/api/comments');if(!r.ok)throw new Error();comments=await r.json();render();$('#status').textContent='Connected · comments save directly to the workspace.';}catch{$('#status').textContent='Start server.py to enable shared feedback. Notes cannot save in file-only mode.';}}
  load();
})();
