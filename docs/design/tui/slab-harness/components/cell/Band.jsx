import React from "react";
import { Span } from "./Span.jsx";
function hLen(segs) { return segs.reduce((n, s) => n + String(s[1] ?? "").length, 0); }
function hSeg(s, i) { const [tone, text, o = {}] = s; return <Span key={i} tone={tone} bold={o.bold} inverse={o.inverse} bg={o.bg}>{text}</Span>; }
export function Band({ bg = "transparent", left = [], right = [], width = 76, pad = 2, minGap = 2 }) {
  const U = width - pad * 2;
  let L = left.map(s => [...s]);
  const rl = hLen(right);
  const need = right.length ? minGap : 0;
  let over = hLen(L) + rl + need - U;
  for (let i = L.length - 1; over > 0 && i >= 0; i--) {
    const t = String(L[i][1]);
    if (t.length > over + 1) { L[i][1] = t.slice(0, t.length - over - 1) + "…"; over = 0; } else { over -= t.length; L[i][1] = ""; }
  }
  const gap = Math.max(0, U - hLen(L) - rl);
  return (
    <div style={{ whiteSpace: "pre", background: bg, width: width + "ch", overflow: "hidden" }}>
      {" ".repeat(pad)}{L.map(hSeg)}{" ".repeat(gap)}{right.map(hSeg)}{" ".repeat(pad)}
    </div>
  );
}
