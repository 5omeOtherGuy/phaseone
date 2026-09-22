import React from "react";
import { Band } from "../cell/Band.jsx";
export function Decision({ options = [], secondary = [], width = 76 }) {
  const left = [];
  options.forEach((o, i) => {
    if (i) left.push(["ink", "   "]);
    left.push(o.disabled ? ["faint", " " + o.key + " "] : ["attn", " " + o.key + " ", { inverse: true }]);
    left.push([o.disabled ? "faint" : "ink", " " + o.label + (o.reason ? "  " + o.reason : "")]);
  });
  const right = secondary.flatMap((s, i) => (i ? [["faint", "   "]] : []).concat([["faint", s]]));
  return <Band bg="var(--h-block-plus)" width={width} left={left} right={right} />;
}
