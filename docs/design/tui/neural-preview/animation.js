/* A seeded network and explicit clock make every annotated frame reproducible.
   All coordinates are logical: a future TUI renderer can sample the same field. */
(() => {
  'use strict';
  const canvas = document.querySelector('#brain'), ctx = canvas.getContext('2d');
  const W = 1000, H = 660, nodes = [], edges = [];
  let seed = 7131;
  const random = () => ((seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0) / 4294967296);
  const clamp = x => Math.max(0, Math.min(1, x));
  const smooth = (a,b,x) => { const t=clamp((x-a)/(b-a)); return t*t*(3-2*t); };
  // Frontal silhouette: scalloped hemispheres, central fissure, narrowing lower lobes.
  function inside(x,y) {
    const side = x < 500 ? -1 : 1;
    const cy = 301, cx = 500 + side*139;
    const angle = Math.atan2((y-cy)/228,(x-cx)/183);
    const scallop = 1 + .038*Math.sin(angle*11) + .018*Math.cos(angle*19);
    const ellipse = ((x-cx)/183)**2 + ((y-cy)/228)**2;
    const cleft = 7+5*Math.sin(y*.036)+3*Math.sin(y*.083);
    return ellipse < scallop && Math.abs(x-500)>cleft && !(y>469 && Math.abs(x-500)>235-(y-469)*1.7);
  }
  // Geometric lowercase p1. Membership selects neurons; no letters are drawn.
  const segment=(x,y,ax,ay,bx,by) => {const dx=bx-ax,dy=by-ay,t=clamp(((x-ax)*dx+(y-ay)*dy)/(dx*dx+dy*dy));return Math.hypot(x-ax-t*dx,y-ay-t*dy);};
  function logo(x,y) {
    const stem=segment(x,y,359,254,359,424)<16;
    const bowl=Math.abs(Math.hypot((x-389)*1.02,y-290)-49)<15 && x>357;
    const one=segment(x,y,588,260,588,388)<16 || segment(x,y,556,281,588,257)<15 || segment(x,y,557,393,619,393)<14;
    return stem||bowl||one;
  }
  // Curved bands resemble cortical folds without turning the brain into a solid fill.
  function fold(x,y) {
    const u=Math.abs(x-500),v=y-295;
    return Math.sin(u*.065+Math.sin(v*.026)*2.8+Math.sin((u+v)*.018)*1.8);
  }
  for(let y=70;y<548;y+=5) for(let x=171;x<830;x+=5) {
    const px=x+(random()-.5)*4,py=y+(random()-.5)*4;
    if(!inside(px,py))continue;
    const f=fold(px,py);
    if(random()>(f>-.25?.89:.37))continue;
    nodes.push({x:px,y:py,logo:logo(px,py),base:.11+.12*(f+1)/2,seed:random(),period:2.4+random()*5});
  }
  const buckets=new Map(),key=(x,y)=>`${Math.floor(x/18)},${Math.floor(y/18)}`;
  nodes.forEach((n,i)=>{const k=key(n.x,n.y);if(!buckets.has(k))buckets.set(k,[]);buckets.get(k).push(i);});
  nodes.forEach((n,i)=>{
    const bx=Math.floor(n.x/18),by=Math.floor(n.y/18),near=[];
    for(let dy=-1;dy<=1;dy++)for(let dx=-1;dx<=1;dx++)for(const j of buckets.get(`${bx+dx},${by+dy}`)||[]){
      if(j<=i)continue;const m=nodes[j],d=Math.hypot(n.x-m.x,n.y-m.y);
      if(d<17 && (n.x-500)*(m.x-500)>0)near.push({j,d});
    }
    near.sort((a,b)=>a.d-b.d);
    for(const {j} of near.slice(0,2))edges.push([i,j,random()]);
  });
  const field=document.createElement('canvas');field.width=W;field.height=H;
  const fctx=field.getContext('2d',{willReadFrequently:true});
  let time=0,playing=!matchMedia('(prefers-reduced-motion: reduce)').matches,mode='fine',last=0;
  function draw(t) {
    time=((t%12)+12)%12;
    const p=smooth(4.4,5.7,time)*(1-smooth(8,10.3,time));
    fctx.clearRect(0,0,W,H);
    const values=nodes.map(n=>{
      const cycle=(time+n.seed*n.period)%n.period;
      const spontaneous=Math.exp(-(((cycle-.16)/.105)**2))*(.3+n.seed*.45);
      const distance=Math.hypot((n.x-500)*.85,n.y-305);
      const wave=Math.exp(-(((distance-(time-4.2)*310)/35)**2))*.55;
      const reveal=smooth(4.65,5.65,time-(n.x-350)*.001)*(1-smooth(8.05,9.7,time-n.seed*.3));
      return clamp(n.base*(1-p*.65)+spontaneous*(1-p*.9)+wave+(n.logo?reveal*(.71+.19*Math.sin(n.seed*7+time*1.2)**2):0));
    });
    // Batch brightness bands so thousands of connections share sixteen draw calls.
    const paths=Array.from({length:16},()=>new Path2D());
    fctx.lineWidth=.65;
    for(const [i,j,s] of edges){
      const a=nodes[i],b=nodes[j],v=Math.min(values[i],values[j]);
      const path=paths[Math.min(15,Math.floor(v*16))];path.moveTo(a.x,a.y);path.lineTo(b.x,b.y);
      const q=(time*.6+s*9)%3;
      if(q<1 && s>.86 && p<.9){fctx.fillStyle=`rgba(255,255,255,${.7*(1-p)})`;fctx.fillRect(a.x+(b.x-a.x)*q,a.y+(b.y-a.y)*q,1.5,1.5);}
    }
    paths.forEach((path,i)=>{fctx.strokeStyle=`rgba(232,232,232,${(i+.5)/16*.48})`;fctx.stroke(path);});
    nodes.forEach((n,i)=>{
      const v=values[i];
      if(v>.58){const glow=fctx.createRadialGradient(n.x,n.y,0,n.x,n.y,5);glow.addColorStop(0,`rgba(240,240,240,${v*.16})`);glow.addColorStop(1,'rgba(240,240,240,0)');fctx.fillStyle=glow;fctx.fillRect(n.x-5,n.y-5,10,10);}
      fctx.fillStyle=`rgba(240,240,240,${v})`;const size=v>.65?1.65:1.1;fctx.fillRect(n.x-size/2,n.y-size/2,size,size);
    });
    ctx.fillStyle='#0a0a0a';ctx.fillRect(0,0,W,H);
    if(mode==='fine')ctx.drawImage(field,0,0);
    else {
      // 240 × 160 dot samples = a 120 × 40 Braille-cell budget. Browser approximation,
      // not a promise about terminal glyph metrics or per-cell colour fidelity.
      const pixels=fctx.getImageData(0,0,W,H).data;
      for(let y=0;y<160;y++)for(let x=0;x<240;x++){
        let v=0;
        for(let dy=0;dy<4;dy++)for(let dx=0;dx<4;dx++)v=Math.max(v,pixels[((Math.floor(y*H/160)+dy)*W+Math.floor(x*W/240)+dx)*4+3]||0);
        if(v<16)continue;ctx.fillStyle=`rgb(${Math.round(10+v*.91)},${Math.round(10+v*.91)},${Math.round(10+v*.91)})`;
        ctx.fillRect(x*W/240,y*H/160,1.8,1.8);
      }
    }
    document.querySelector('#time').textContent=time.toFixed(2)+' s';
    document.querySelector('#scrub').value=time;
    document.querySelector('#phase').textContent=time<4.4?'SPONTANEOUS ACTIVITY':time<5.8?'SYNCHRONISING':time<8?'A THOUGHT / p1':time<10.3?'RELEASING':'SPONTANEOUS ACTIVITY';
  }
  function pause(){playing=false;document.querySelector('#play').textContent='Play';}
  window.neural={get time(){return time;},get mode(){return mode;},pause,draw,setMode(v){mode=v;document.querySelector('#mode').value=v;draw(time);},nodes:nodes.length,edges:edges.length};
  document.querySelector('#play').onclick=()=>{playing=!playing;document.querySelector('#play').textContent=playing?'Pause':'Play';};
  document.querySelector('#pulse').onclick=()=>{pause();draw(6.6);};
  document.querySelector('#scrub').oninput=e=>{pause();draw(+e.target.value);};
  document.querySelector('#mode').onchange=e=>{mode=e.target.value;draw(time);};
  document.querySelector('#focus').onclick=e=>{const active=document.body.classList.toggle('fullscreen');e.target.setAttribute('aria-pressed',active);};
  const params=new URLSearchParams(location.search);
  if(params.has('t')){pause();time=Number(params.get('t'))||0;}
  if(params.get('mode')==='terminal')window.neural.setMode('terminal');
  draw(time);if(!playing)pause();
  function tick(now){if(playing&&!document.hidden)draw(time+Math.min((now-last)/1000,.05));last=now;requestAnimationFrame(tick);}
  requestAnimationFrame(tick);
})();
