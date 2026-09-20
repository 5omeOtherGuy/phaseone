/* A closed 24-second orbit. Every changing term is periodic in the same phase:
   geometry, signals, agent orbits and message travel all cross the seam smoothly. */
(() => {
  const TAU=Math.PI*2,PERIOD=24,clamp=x=>Math.max(0,Math.min(1,x));
  let seed=10841;
  const random=()=>((seed=(Math.imul(seed,1664525)+1013904223)>>>0)/4294967296);
  const nodes=[],edges=[],count=4300;
  for(let i=0;i<count;i++){
    const y=1-2*(i+.5)/count,a=i*2.399963229728653,r=Math.sqrt(1-y*y);
    const layer=.975+.025*random(),x=Math.cos(a)*r,z=Math.sin(a)*r;
    const fold=Math.sin(Math.abs(x)*15+Math.sin(y*7)*2.4+Math.sin(z*9)*1.3);
    const ridge=1-.045*(fold+1),cleft=Math.exp(-((x/.085)**2));
    nodes.push({x,y,z,layer,fold,seed:random(),brain:[x*183*ridge,y*220*ridge,z*151*ridge-Math.max(0,z)*29*cleft],sphere:[x*192,y*192,z*192]});
  }
  const bins=new Map(),cell=24,key=(x,y,z)=>`${x},${y},${z}`;
  nodes.forEach((n,i)=>{const k=key(...n.brain.map(v=>Math.floor(v/cell)));if(!bins.has(k))bins.set(k,[]);bins.get(k).push(i);});
  nodes.forEach((n,i)=>{
    const [bx,by,bz]=n.brain.map(v=>Math.floor(v/cell)),near=[];
    for(let dz=-1;dz<=1;dz++)for(let dy=-1;dy<=1;dy++)for(let dx=-1;dx<=1;dx++)for(const j of bins.get(key(bx+dx,by+dy,bz+dz))||[]){
      if(j<=i)continue;const m=nodes[j],d=Math.hypot(...n.brain.map((v,k)=>v-m.brain[k]));if(d<29)near.push({j,d});
    }
    near.sort((a,b)=>a.d-b.d);
    for(const {j} of near.slice(0,3))edges.push([i,j,random()]);
  });
  const logoDoc=new DOMParser().parseFromString(phaseoneLogos.svg('synapse','p1'),'image/svg+xml');
  const logoNodes=[...logoDoc.querySelectorAll('circle')].map(n=>({x:+n.getAttribute('cx'),y:+n.getAttribute('cy')}));
  const logoEdges=[];
  logoNodes.forEach((a,i)=>logoNodes.forEach((b,j)=>{if(j>i&&Math.abs(Math.hypot(a.x-b.x,a.y-b.y)-12)<.01)logoEdges.push([i,j]);}));
  const logoMark=phaseoneLogos.mark('synapse','p1');
  const glow=document.createElement('canvas');glow.width=glow.height=24;
  const gc=glow.getContext('2d'),gradient=gc.createRadialGradient(12,12,0,12,12,12);gradient.addColorStop(0,'rgba(240,240,240,.34)');gradient.addColorStop(1,'rgba(240,240,240,0)');gc.fillStyle=gradient;gc.fillRect(0,0,24,24);
  function pose(t,morph=true){const phase=TAU*t/PERIOD;return {phase,yaw:phase+.15,pitch:.18,morph:morph?(1-Math.cos(phase))/2:0};}
  function project(x,y,z){const k=900/(900-z);return {x:500+x*k,y:302+y*k,z,k};}
  function rotate(x,y,z,p){const rx=x*Math.cos(p.yaw)+z*Math.sin(p.yaw),rz=z*Math.cos(p.yaw)-x*Math.sin(p.yaw);return project(rx,y*Math.cos(p.pitch)-rz*Math.sin(p.pitch),y*Math.sin(p.pitch)+rz*Math.cos(p.pitch));}
  function positions(t,settings){const p=pose(settings.motion?t:0,settings.morph);return nodes.map(n=>{
    const xyz=n.brain.map((v,k)=>(v*(1-p.morph)+n.sphere[k]*p.morph)*(n.layer*(1-p.morph)+p.morph));
    return rotate(...xyz,p);
  });}
  const bezier=(a,b,c,d,u)=>({x:(1-u)**3*a.x+3*(1-u)**2*u*b.x+3*(1-u)*u*u*c.x+u**3*d.x,y:(1-u)**3*a.y+3*(1-u)**2*u*b.y+3*(1-u)*u*u*c.y+u**3*d.y});
  function agentCenters(t){const phase=TAU*t/PERIOD;return Array.from({length:3},(_,i)=>{
    const a=phase+i*TAU/3,r=315+18*Math.sin(a*2),x=Math.cos(a)*r,z=Math.sin(a)*240,y=Math.sin(a+.25)*145+(i-1)*22;
    return {...project(x,y,z),radius:20+i*6,phase:a,index:i};
  });}
  function drawThreads(ctx,t,settings,agents){
    if(!settings.threads)return;
    const phase=TAU*(settings.motion?t:0)/PERIOD;
    const anchors=agents.map((a,i)=>({x:500+Math.cos(phase+i*2.1)*110,y:302+Math.sin(phase+i*2.1)*125}));
    function thread(a,d,bend,offset){
      const mid={x:(a.x+d.x)/2,y:(a.y+d.y)/2},b={x:mid.x+bend,y:a.y-bend},c={x:mid.x-bend*.3,y:d.y+bend};
      ctx.beginPath();ctx.moveTo(a.x,a.y);ctx.bezierCurveTo(b.x,b.y,c.x,c.y,d.x,d.y);ctx.strokeStyle='rgba(210,210,210,.16)';ctx.lineWidth=.7;ctx.stroke();
      if(settings.signals)for(let k=0;k<3;k++){
        const u=((t/PERIOD*2+offset+k/3)%1+1)%1,point=bezier(a,b,c,d,u),alpha=Math.sin(Math.PI*u)**2*.88;
        ctx.globalAlpha=alpha;ctx.drawImage(glow,point.x-6,point.y-6,12,12);ctx.fillStyle='#e8e8e8';ctx.beginPath();ctx.arc(point.x,point.y,1.7,0,TAU);ctx.fill();ctx.globalAlpha=1;
      }
    }
    agents.forEach((a,i)=>thread(a,anchors[i],(i%2?1:-1)*90,i/3));
    agents.forEach((a,i)=>thread(a,agents[(i+1)%3],i%2?80:-90,.17+i/3));
  }
  function drawAgent(ctx,a,t,settings){
    const local=[];
    for(let i=0;i<88;i++){
      const y=1-2*(i+.5)/88,angle=i*2.39996323+a.phase,r=Math.sqrt(1-y*y);
      local.push({x:a.x+Math.cos(angle)*r*a.radius*a.k,y:a.y+y*a.radius*a.k,z:Math.sin(angle)*r});
    }
    if(settings.connections){ctx.beginPath();local.forEach((n,i)=>{for(const j of [i+8,i+13])if(j<local.length&&Math.hypot(n.x-local[j].x,n.y-local[j].y)<a.radius*.9){ctx.moveTo(n.x,n.y);ctx.lineTo(local[j].x,local[j].y);}});ctx.strokeStyle=`rgba(220,220,220,${.18+.11*a.k})`;ctx.lineWidth=.6;ctx.stroke();}
    if(settings.easterEggs&&(a.index===0||a.index===2)){const visibility=Math.max(0,Math.sin(a.phase))**4*.52;ctx.font='10px ui-monospace, monospace';ctx.fillStyle=`rgba(220,220,220,${visibility})`;ctx.textAlign='center';ctx.fillText(a.index===0?'10841':'[big]',a.x,a.y+a.radius*a.k+14);}
    if(settings.neurons)local.forEach((n,i)=>{const signal=settings.signals?Math.max(0,Math.cos(TAU*t/PERIOD*3-i*.4))**16:0;ctx.fillStyle=`rgba(240,240,240,${.2+.22*(n.z+1)/2+signal*.5})`;ctx.fillRect(n.x,n.y,1.4*a.k,1.4*a.k);});
  }
  function draw(ctx,t,settings){
    const phase=TAU*t/PERIOD,points=positions(t,settings),agents=settings.agents?agentCenters(settings.motion?t:0):[];
    ctx.clearRect(0,0,1000,660);
    if(settings.halo){
      // Broken meridians establish depth without drawing a hard circular border.
      for(let orbit=0;orbit<2;orbit++){
        ctx.beginPath();for(let i=0;i<=150;i++){const a=i/150*TAU,r=235+orbit*21;const n=rotate(Math.cos(a)*r,Math.sin(a)*r*.66,Math.sin(a)*r*.58,{yaw:(settings.motion?phase:0)+orbit*1.1,pitch:.24});if(i===0)ctx.moveTo(n.x,n.y);else ctx.lineTo(n.x,n.y);}ctx.strokeStyle=`rgba(220,220,220,${orbit?.045:.07})`;ctx.lineWidth=.6;ctx.stroke();
      }
    }
    drawThreads(ctx,t,settings,agents);agents.filter(a=>a.z<0).forEach(a=>drawAgent(ctx,a,t,settings));
    const values=nodes.map((n,i)=>{
      const a=points[i],depth=clamp((a.z+215)/430),wave=Math.max(0,Math.cos(phase*3-n.y*5-n.x*3+n.z*2))**26;
      const firing=Math.max(0,Math.cos(phase*(2+Math.floor(n.seed*4))+n.seed*TAU))**50;
      const reserve=settings.identity?(.43+.57*clamp((Math.hypot(a.x-500,a.y-300)-65)/75)):1;
      return clamp(((.13+.1*(n.fold+1)/2)*(.32+depth*.85)+(settings.signals?(wave*.32+firing*.4)*(.45+depth*.5):0))*settings.strength*reserve);
    });
    if(settings.connections){
      const paths=Array.from({length:16},()=>new Path2D());
      for(const [i,j,s] of edges){const a=points[i],b=points[j],v=Math.min(values[i],values[j]);const path=paths[Math.min(15,Math.floor(v*16))];path.moveTo(a.x,a.y);path.lineTo(b.x,b.y);}
      paths.forEach((path,i)=>{ctx.strokeStyle=`rgba(226,226,226,${(i+.5)/16*.7})`;ctx.lineWidth=.65;ctx.stroke(path);});
    }
    if(settings.neurons){
      const paths=Array.from({length:16},()=>new Path2D());
      nodes.forEach((n,i)=>{const a=points[i],v=values[i],size=(v>.4?1.6:1.1)*a.k;paths[Math.min(15,Math.floor(v*16))].rect(a.x-size/2,a.y-size/2,size,size);if(v>.48){ctx.globalAlpha=v*.8;ctx.drawImage(glow,a.x-5,a.y-5,10,10);}});ctx.globalAlpha=1;
      paths.forEach((path,i)=>{ctx.fillStyle=`rgba(240,240,240,${(i+.5)/16})`;ctx.fill(path);});
    }
    if(settings.identity){
      const scale=1.2*settings.logoScale;
      const logo=logoNodes.map(n=>({x:500+(n.x-logoMark.width/2)*scale,y:302+(n.y-66)*scale}));
      ctx.strokeStyle='rgba(235,235,235,.53)';ctx.lineWidth=1.05;ctx.beginPath();for(const [i,j] of logoEdges){ctx.moveTo(logo[i].x,logo[i].y);ctx.lineTo(logo[j].x,logo[j].y);}ctx.stroke();
      if(settings.threads){ctx.strokeStyle='rgba(220,220,220,.075)';ctx.lineWidth=.6;ctx.beginPath();for(let i=0;i<logo.length;i+=6){const a=logo[i],b=points[(i*97+211)%points.length];ctx.moveTo(a.x,a.y);ctx.quadraticCurveTo((a.x+b.x)/2+20,(a.y+b.y)/2-20,b.x,b.y);}ctx.stroke();}
      logo.forEach(a=>{ctx.globalAlpha=.5;ctx.drawImage(glow,a.x-8,a.y-8,16,16);ctx.globalAlpha=1;ctx.fillStyle='#e8e8e8';ctx.beginPath();ctx.arc(a.x,a.y,3.1*scale,0,TAU);ctx.fill();});
    }
    agents.filter(a=>a.z>=0).forEach(a=>drawAgent(ctx,a,t,settings));
    return {morph:pose(settings.motion?t:0,settings.morph).morph};
  }
  window.awakening={draw,positions,agentCenters,pose,period:PERIOD,nodes:nodes.length+264,edges:edges.length};
})();
