/* Each study has its own seeded geometry. No raster assets or external libraries. */
window.neuralStudies = [
  {id:'cortex',name:'01 / Cortex',detail:'Compact · top-down',description:'A close-set oval cortex. Fine winding folds and a narrow central fissure; the brain reads as one compact form.'},
  {id:'field',name:'02 / Neural field',detail:'Abstract · living filaments',description:'No anatomical silhouette. Dendrites, branching signals and synchronised firing suggest an intelligence without drawing a brain.'},
  {id:'cortex3d',name:'03 / Living cortex',detail:'3D · turning volume',description:'A compact cortical volume turns and breathes. Near and far neurons separate through perspective, brightness and motion.'},
  {id:'nebula',name:'04 / Neural nebula',detail:'3D · flowing & morphing',description:'An abstract volume folds through itself: a slowly turning, breathing neural cloud. The p1 pulse emerges within its moving projection.'},
  {id:'web',name:'05 / Synaptic web',detail:'Expansive · connections first',description:'An open, far-reaching web. Small neuron clusters anchor long axons, with impulses travelling visibly between them.'},
  {id:'lightning',name:'06 / Dendritic lightning',detail:'Expansive · electric branches',description:'Angular dendrites radiate into space. Local discharges sweep down branching paths; a coordinated pulse gathers into p1.'},
  {id:'original',name:'00 / Original',detail:'First study · reference',description:'The original wide silhouette, kept for comparison and replaying comments from the first study.'}
];
window.createNeuralNetwork = id => {
  if(id==='original')return {...createLegacyNetwork(),id};
  let seed=7131;
  const random=()=>((seed=(Math.imul(seed,1664525)+1013904223)>>>0)/4294967296);
  const nodes=[],edges=[];
  const add=(x,y,z=0,extra={})=>{nodes.push({x,y,z,seed:random(),period:2.8+random()*4.5,base:.13+random()*.09,...extra});return nodes.length-1;};
  const fold=(x,y)=>Math.sin(Math.abs(x)*.078+Math.sin(y*.027)*2.5+Math.sin((Math.abs(x)+y)*.017)*1.6);
  if(id==='cortex'){
    for(let y=-239;y<=239;y+=4.5)for(let x=-196;x<=196;x+=4.5){
      const px=x+(random()-.5)*3.6,py=y+(random()-.5)*3.6;
      const a=Math.atan2(py/232,px/183),r=(px/183)**2+(py/232)**2;
      const rim=1+.019*Math.sin(13*a)+.014*Math.cos(21*a);
      const cleft=2.1+1.25*Math.sin(py*.031)+.7*Math.sin(py*.092);
      if(r>rim||Math.abs(px)<cleft)continue;
      const f=fold(px,py);
      if(random()>(f>-.12?.96:.22))continue;
      add(px+500,py+315,0,{base:.12+.14*(f+1)/2});
    }
  }else if(id==='cortex3d'||id==='nebula'){
    // Populate actual XYZ volume; local edges remain attached during projection.
    for(let i=0;i<6200;i++){
      const y=random()*2-1,angle=random()*Math.PI*2;
      const radius=(id==='cortex3d'?.8:.2)+(id==='cortex3d'?.2:.8)*Math.cbrt(random());
      const cross=Math.sqrt(1-y*y),x=Math.cos(angle)*cross,z=Math.sin(angle)*cross;
      const f=fold(x*183,y*225);
      if(id==='cortex3d'&&random()>(f>-.3?.95:.38))continue;
      const dent=id==='cortex3d'?1-.1*Math.exp(-((x/.09)**2)):1;
      add(500+x*183*radius,315+y*225*radius*dent,z*154*radius,{base:.14+.12*(f+1)/2});
    }
  }else if(id==='field'||id==='lightning'){
    // Many dendritic trees overlap; the outer envelope remains open and irregular.
    const hubs=id==='field'?21:13;
    function branch(parent,angle,length,depth,group){
      if(depth===0)return;
      let prev=parent,px=nodes[parent].x,py=nodes[parent].y;
      const steps=Math.max(4,Math.round(length/4));
      for(let j=1;j<=steps;j++){
        const u=j/steps;
        const bend=id==='lightning'?Math.sin(j*2.7+group)*5:Math.sin(u*Math.PI)*8;
        const x=px+Math.cos(angle)*length*u+Math.cos(angle+Math.PI/2)*bend;
        const y=py+Math.sin(angle)*length*u+Math.sin(angle+Math.PI/2)*bend;
        if(x<65||x>935||y<60||y>578)break;
        const n=add(x,y,0,{group,travel:(nodes[parent].travel||0)+u*length,base:id==='lightning'?.11:.19});
        edges.push([prev,n,random()]);prev=n;
      }
      if(prev===parent)return;
      branch(prev,angle+(random()-.5)*.5,length*.68,depth-1,group);
      branch(prev,angle+(random()>.5?1:-1)*(.5+random()*.6),length*.53,depth-1,group);
    }
    for(let h=0;h<hubs;h++){
      const a=h*2.39996,r=Math.sqrt((h+.5)/hubs);
      const x=500+Math.cos(a)*r*(id==='field'?255:310),y=315+Math.sin(a)*r*145;
      const hub=add(x,y,0,{group:h,travel:0,hub:true});
      for(let k=0;k<5;k++)branch(hub,k*Math.PI*2/5+random()*.55,46+random()*41,id==='field'?4:4,h);
    }
    // Sparse bridges make the dendritic forest one connected visual field.
    const roots=nodes.map((n,i)=>n.hub?i:-1).filter(i=>i>=0);
    for(let i=1;i<roots.length;i++)edges.push([roots[i-1],roots[i],random()]);
  }else if(id==='web'){
    const hubs=[];
    for(let h=0;h<62;h++){
      const a=h*2.39996,r=Math.sqrt((h+.3)/62);
      const x=500+Math.cos(a)*r*415,y=315+Math.sin(a)*r*238;
      hubs.push({x,y});
      for(let j=0;j<18;j++){const a=random()*Math.PI*2,r=Math.sqrt(random())*17;add(x+Math.cos(a)*r,y+Math.sin(a)*r,0,{base:.22,hub:j===0});}
    }
    for(let h=0;h<hubs.length;h++){
      const near=hubs.map((n,j)=>({j,d:Math.hypot(n.x-hubs[h].x,n.y-hubs[h].y)})).filter(n=>n.j>h).sort((a,b)=>a.d-b.d).slice(0,3);
      for(const {j} of near){
        const start=h*18,end=j*18,a=nodes[start],b=nodes[end];let prev=start;
        const steps=Math.ceil(Math.hypot(a.x-b.x,a.y-b.y)/5);
        for(let k=1;k<steps;k++){const u=k/steps,sag=Math.sin(u*Math.PI)*12;const n=add(a.x+(b.x-a.x)*u,a.y+(b.y-a.y)*u+sag,0,{base:.2,axon:true});edges.push([prev,n,random()]);prev=n;}
        edges.push([prev,end,random()]);
      }
    }
  }
  if(!['field','lightning'].includes(id)){
    const cell=id.includes('3d')||id==='nebula'?25:18,buckets=new Map();
    const key=(x,y,z)=>`${x},${y},${z}`;
    nodes.forEach((n,i)=>{const k=key(Math.floor(n.x/cell),Math.floor(n.y/cell),Math.floor(n.z/cell));if(!buckets.has(k))buckets.set(k,[]);buckets.get(k).push(i);});
    nodes.forEach((n,i)=>{
      if(n.axon)return;
      const bx=Math.floor(n.x/cell),by=Math.floor(n.y/cell),bz=Math.floor(n.z/cell),near=[];
      for(let dz=-1;dz<=1;dz++)for(let dy=-1;dy<=1;dy++)for(let dx=-1;dx<=1;dx++)for(const j of buckets.get(key(bx+dx,by+dy,bz+dz))||[]){
        if(j<=i||nodes[j].axon)continue;const m=nodes[j];
        if(id==='cortex'&&(n.x-500)*(m.x-500)<0)continue;
        const d=Math.hypot(n.x-m.x,n.y-m.y,n.z-m.z);if(d<cell*1.5)near.push({j,d});
      }
      near.sort((a,b)=>a.d-b.d);
      for(const {j} of near.slice(0,id==='web'?2:3))edges.push([i,j,random()]);
    });
  }
  return {id,nodes,edges};
};
