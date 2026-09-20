/* Explicit time + seeded geometry let comments replay a specific study and frame. */
(() => {
  'use strict';
  const $=s=>document.querySelector(s),canvas=$('#brain'),ctx=canvas.getContext('2d');
  const W=1000,H=660,cache=new Map();
  const field=document.createElement('canvas');field.width=W;field.height=H;
  const fctx=field.getContext('2d',{willReadFrequently:true});
  const glow=document.createElement('canvas');glow.width=glow.height=16;
  const gc=glow.getContext('2d'),gradient=gc.createRadialGradient(8,8,0,8,8,8);
  gradient.addColorStop(0,'rgba(240,240,240,.3)');gradient.addColorStop(1,'rgba(240,240,240,0)');gc.fillStyle=gradient;gc.fillRect(0,0,16,16);
  const clamp=x=>Math.max(0,Math.min(1,x));
  const smooth=(a,b,x)=>{const t=clamp((x-a)/(b-a));return t*t*(3-2*t);};
  let time=0,playing=!matchMedia('(prefers-reduced-motion: reduce)').matches,mode='fine',variant='cortex',network,last=0,cycle=0,assignmentKey='',assignment=new Map();
  function project(n,t,p){
    const x=n.x-500,y=n.y-315,z=n.z||0,phase=t*Math.PI/6;
    if(variant==='cortex3d'||variant==='nebula'){
      const nebula=variant==='nebula';
      const yaw=nebula?phase:.62*Math.sin(phase)+.2;
      const pitch=nebula?.36*Math.sin(phase):.26+.18*Math.cos(phase);
      const morph=nebula?1+.22*Math.sin(phase*2+y*.014)+.11*Math.cos(z*.027-phase):1+.026*Math.sin(phase*2+y*.021);
      const twist=nebula?.5*Math.sin(phase+y*.01):0;
      const xx=(x*Math.cos(twist)-z*Math.sin(twist))*morph;
      const zz=(x*Math.sin(twist)+z*Math.cos(twist))*morph;
      const rx=xx*Math.cos(yaw)+zz*Math.sin(yaw),rz=zz*Math.cos(yaw)-xx*Math.sin(yaw);
      const ry=y*Math.cos(pitch)-rz*Math.sin(pitch),depth=y*Math.sin(pitch)+rz*Math.cos(pitch);
      const perspective=850/(850-depth),breath=nebula?1.17:1;
      return {x:500+rx*perspective*breath,y:315+ry*perspective,depth:clamp((depth+220)/440),scale:perspective};
    }
    const drift=variant==='field'?3.5:variant==='web'?1.8:0;
    return {x:n.x+drift*Math.sin(phase+y*.013),y:n.y+drift*Math.cos(phase+x*.014),depth:1,scale:1};
  }
  function assignLogo(points){
    const key=variant+':'+playground.maskVersion;
    if(key===assignmentKey)return;
    assignmentKey=key;assignment=new Map();
    const bins=new Map(),size=25;
    points.forEach((n,i)=>{const k=`${Math.floor(n.x/size)},${Math.floor(n.y/size)}`;if(!bins.has(k))bins.set(k,[]);bins.get(k).push(i);});
    for(const target of playground.targets){
      const bx=Math.floor(target.x/size),by=Math.floor(target.y/size);let closest=-1,best=Infinity;
      for(let radius=0;radius<18;radius++){
        for(let dy=-radius;dy<=radius;dy++)for(let dx=-radius;dx<=radius;dx++){
          if(radius&&Math.abs(dx)!==radius&&Math.abs(dy)!==radius)continue;
          for(const i of bins.get(`${bx+dx},${by+dy}`)||[]){if(assignment.has(i))continue;const n=points[i],d=(n.x-target.x)**2+(n.y-target.y)**2;if(d<best){closest=i;best=d;}}
        }
        if(closest>=0&&Math.sqrt(best)<radius*size)break;
      }
      if(closest>=0)assignment.set(closest,target);
    }
  }
  function draw(t){
    if(t>=12)cycle=(cycle+Math.floor(t/12))%2;
    time=((t%12)+12)%12;
    const settings=playground.settings;playground.setActiveWord(cycle);
    const {nodes,edges}=network,p=settings.pulse?smooth(4.4,5.7,time)*(1-smooth(8,10.3,time)):0;
    const spatial=variant==='cortex3d'||variant==='nebula';
    const points=nodes.map(n=>project(n,settings.motion?time:0,p));
    if(settings.gather&&variant!=='original'){
      assignLogo(nodes.map(n=>project(n,settings.motion?6.6:0,1)));
      for(const [i,target] of assignment){const n=points[i];n.x+=(target.x-n.x)*p;n.y+=(target.y-n.y)*p;}
    }
    const values=nodes.map((n,i)=>{
      const point=points[i],period=variant==='original'?n.period:12/(2+Math.floor(n.seed*4));
      const cycle=(time+n.seed*period)%period;
      const spontaneous=Math.exp(-(((cycle-.16)/.12)**2))*(.35+n.seed*.45);
      const distance=Math.hypot((point.x-500)*.85,point.y-305);
      const wave=Math.exp(-(((distance-(time-4.2)*310)/35)**2))*.55;
      const reveal=smooth(4.65,5.65,time-(point.x-350)*.001)*(1-smooth(8.05,9.7,time-n.seed*.3));
      const member=variant==='original'?n.logo:settings.gather?assignment.has(i):playground.contains(point.x,point.y);
      const depth=spatial?.23+point.depth*.85:1;
      let discharge=0;
      if(n.group!==undefined){const front=(time*75+n.group*31)%300;discharge=Math.exp(-(((n.travel-front)/19)**2))*(variant==='lightning'?.88:.55);}
      return clamp((n.base*depth*(1-p*.55)+(settings.signals?(spontaneous+discharge)*depth*(1-p*.84):0)+(settings.pulse?wave*depth:0)+(member&&settings.pulse?reveal*(.76+.16*Math.sin(n.seed*7+time*1.2)**2):0))*settings.strength);
    });
    fctx.clearRect(0,0,W,H);
    const paths=Array.from({length:20},()=>new Path2D()),sparks=[];
    const electric=variant==='lightning',web=variant==='web';
    for(const [i,j,s] of settings.connections?edges:[]){
      const a=points[i],b=points[j];
      let v=Math.min(values[i],values[j]);
      if(p>.5&&variant!=='original'&&(!playground.contains((a.x+b.x)/2,(a.y+b.y)/2)||!playground.contains(a.x*.75+b.x*.25,a.y*.75+b.y*.25)))v=Math.min(v,.13);
      const path=paths[Math.min(19,Math.floor(v*20))];path.moveTo(a.x,a.y);path.lineTo(b.x,b.y);
      const q=(time*.65+s*9)%3;
      if(settings.signals&&q<1&&s>.9&&p<.9)sparks.push([a.x+(b.x-a.x)*q,a.y+(b.y-a.y)*q]);
    }
    paths.forEach((path,i)=>{
      const v=(i+.5)/20;
      if(electric&&i>9){fctx.lineWidth=4;fctx.strokeStyle=`rgba(232,232,232,${v*.09})`;fctx.stroke(path);}
      fctx.lineWidth=electric?.85:web?.8:.6;fctx.strokeStyle=`rgba(232,232,232,${v*(electric?.94:web?.85:.54)})`;fctx.stroke(path);
    });
    fctx.fillStyle=`rgba(255,255,255,${.75*(1-p)})`;
    for(const [x,y] of sparks)fctx.fillRect(x,y,1.5,1.5);
    const dots=Array.from({length:20},()=>new Path2D());
    if(settings.neurons)nodes.forEach((n,i)=>{
      const v=values[i],a=points[i];
      if(v>.62){fctx.globalAlpha=v*(spatial?.75:1);fctx.drawImage(glow,a.x-5,a.y-5,10,10);}
      const size=(n.hub?2.8:v>.65?1.65:1.05)*a.scale;
      dots[Math.min(19,Math.floor(v*20))].rect(a.x-size/2,a.y-size/2,size,size);
    });
    fctx.globalAlpha=1;
    dots.forEach((path,i)=>{fctx.fillStyle=`rgba(240,240,240,${(i+.5)/20})`;fctx.fill(path);});
    ctx.fillStyle='#0a0a0a';ctx.fillRect(0,0,W,H);
    if(settings.view==='logo')ctx.drawImage(playground.mask,0,0);
    else if(mode==='fine')ctx.drawImage(field,0,0);
    else {
      const pixels=fctx.getImageData(0,0,W,H).data;
      for(let y=0;y<160;y++)for(let x=0;x<240;x++){
        let v=0;
        for(let dy=0;dy<4;dy++)for(let dx=0;dx<4;dx++)v=Math.max(v,pixels[((Math.floor(y*H/160)+dy)*W+Math.floor(x*W/240)+dx)*4+3]||0);
        if(v<16)continue;const c=Math.round(10+v*.91);ctx.fillStyle=`rgb(${c},${c},${c})`;ctx.fillRect(x*W/240,y*H/160,1.8,1.8);
      }
    }
    $('#time').textContent=time.toFixed(2)+' s';$('#scrub').value=time;
    $('#phase').textContent=settings.view==='logo'?'IDENTITY STUDY':!settings.pulse?'SPONTANEOUS ACTIVITY':time<4.4?'SPONTANEOUS ACTIVITY':time<5.8?'SYNCHRONISING':time<8?'IDENTITY PULSE':time<10.3?'RELEASING':'SPONTANEOUS ACTIVITY';
  }
  function pause(){playing=false;$('#play').textContent='Play';}
  function setVariant(id){
    if(!neuralStudies.some(s=>s.id===id))id='cortex';
    variant=id;assignmentKey='';if(!cache.has(id))cache.set(id,createNeuralNetwork(id));network=cache.get(id);
    const study=neuralStudies.find(s=>s.id===id);$('#study-description').textContent=study.description;
    $('#study-name').textContent=study.name;$('#network-count').textContent=`${network.nodes.length.toLocaleString()} neurons · ${network.edges.length.toLocaleString()} connections`;
    document.querySelectorAll('[data-variant]').forEach(b=>b.setAttribute('aria-pressed',b.dataset.variant===id));
    const url=new URL(location.href);url.searchParams.set('variant',id);history.replaceState(null,'',url);
    draw(time);document.dispatchEvent(new Event('variantchange'));
  }
  window.neural={get cycle(){return cycle;},setCycle(v){cycle=v===1?1:0;},get time(){return time;},get mode(){return mode;},get variant(){return variant;},get nodes(){return network.nodes.length;},get edges(){return network.edges.length;},pause,draw,setVariant,setMode(v){mode=v;$('#mode').value=v;draw(time);}};
  for(const study of neuralStudies){
    const b=document.createElement('button');b.dataset.variant=study.id;b.setAttribute('aria-pressed','false');
    const name=document.createElement('strong'),detail=document.createElement('small');name.textContent=study.name;detail.textContent=study.detail;b.append(name,detail);
    b.onclick=()=>setVariant(study.id);$('#variants').append(b);
  }
  $('#play').onclick=()=>{playing=!playing;$('#play').textContent=playing?'Pause':'Play';};
  $('#pulse').onclick=()=>{pause();draw(6.6);};$('#scrub').oninput=e=>{pause();draw(+e.target.value);};
  $('#mode').onchange=e=>neural.setMode(e.target.value);
  $('#focus').onclick=e=>{const active=document.body.classList.toggle('fullscreen');e.target.setAttribute('aria-pressed',active);};
  const params=new URLSearchParams(location.search);
  if(params.has('t')){pause();time=Number(params.get('t'))||0;}
  if(params.get('mode')==='terminal')mode='terminal';$('#mode').value=mode;
  cycle=params.get('cycle')==='1'?1:0;
  document.addEventListener('settingschange',()=>{assignmentKey='';draw(time);});document.addEventListener('maskready',()=>{assignmentKey='';draw(time);});
  setVariant(params.get('variant')||'cortex');if(!playing)pause();
  function tick(now){if(now-last>=1000/30){if(playing&&!document.hidden)draw(time+Math.min((now-last)/1000,.1)*playground.settings.speed);last=now;}requestAnimationFrame(tick);}
  requestAnimationFrame(tick);
})();
