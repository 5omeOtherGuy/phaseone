/* A seeded network and explicit clock make every annotated frame reproducible.
   All coordinates are logical: a future TUI renderer can sample the same field. */
window.createLegacyNetwork = () => {
  'use strict';
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
  return {nodes,edges};
};
