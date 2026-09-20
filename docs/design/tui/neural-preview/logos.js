/* Original vector marks: a lowercase p paired with a deliberately taller numeral.
   The stepped family nods to Pi's modular mark; all geometry here is newly drawn. */
(() => {
  const pixels={
    p:['00000','00000','11110','10001','10001','10001','11110','10000','10000'],
    '1':['00100','01100','00100','00100','00100','00100','01110','00000','00000'],
    h:['10000','10000','10110','11001','10001','10001','10001','00000','00000'],
    a:['00000','00000','01110','00001','01111','10001','01111','00000','00000'],
    s:['00000','00000','01111','10000','01110','00001','11110','00000','00000'],
    e:['00000','00000','01110','10001','11111','10000','01111','00000','00000'],
    o:['00000','00000','01110','10001','10001','10001','01110','00000','00000'],
    n:['00000','00000','10110','11001','10001','10001','10001','00000','00000']
  };
  const strokes={
    p:'M7 42V112 M7 51Q14 40 27 42Q47 42 47 65Q47 88 27 88Q14 88 7 79',
    h:'M7 18V88 M7 56Q12 42 28 42Q47 42 47 61V88',
    a:'M9 47Q48 30 47 60V88 M47 60H24Q6 60 7 76Q8 94 28 88L47 78',
    s:'M46 47Q34 38 19 43Q3 48 9 59Q13 65 29 66Q50 68 46 80Q40 95 8 84',
    e:'M8 65H48Q48 41 28 41Q7 41 7 65Q7 93 44 86',
    o:'M28 42C0 42 0 89 28 89C56 89 56 42 28 42Z',
    n:'M7 43V88 M7 56Q12 42 28 42Q47 42 47 61V88'
  };
  function mark(family,word){
    if(family==='contour'){
      if(word==='p1')return {width:160,height:132,body:'<g fill="none" stroke="#e8e8e8" stroke-width="12" stroke-linecap="square" stroke-linejoin="round"><path d="M20 49V119 M20 58Q33 44 49 49Q73 54 73 75Q73 100 49 100Q32 100 20 88 M103 30L125 13V100 M104 100H146"/></g>'};
      return {width:8*65+12,height:132,body:'<g fill="none" stroke="#e8e8e8" stroke-width="9" stroke-linecap="square" stroke-linejoin="round">'+[...word].map((c,i)=>`<path transform="translate(${i*65+8} 5)" d="${strokes[c]}"/>`).join('')+'</g>'};
    }
    const unit=12,gap=word==='p1'?12:7,width=word.length*(60+gap)-gap+24;
    let body='';
    [...word].forEach((c,i)=>pixels[c].forEach((row,y)=>[...row].forEach((v,x)=>{
      if(v!=='1')return;const px=12+i*(60+gap)+x*unit,py=12+y*unit;
      if(family==='synapse'){
        if(row[x+1]==='1')body+=`<path d="M${px+6} ${py+6}h12"/>`;
        if(pixels[c][y+1]?.[x]==='1')body+=`<path d="M${px+6} ${py+6}v12"/>`;
        body+=`<circle cx="${px+6}" cy="${py+6}" r="3.8" fill="#e8e8e8"/>`;
      }else body+=`<rect x="${px}" y="${py}" width="12" height="12"/>`;
    })));
    return {width,height:132,body:`<g ${family==='synapse'?'fill="none" stroke="#e8e8e8" stroke-width="1.5"':'fill="#e8e8e8"'}>${body}</g>`};
  }
  function svg(family,word){const m=mark(family,word);return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${m.width} ${m.height}" role="img" aria-label="${word} ${family} logo">${m.body}</svg>`;}
  window.phaseoneLogos={mark,svg};
})();
