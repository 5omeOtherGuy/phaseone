import React from "react";
import { Band } from "../cell/Band.jsx";
import { Decision } from "./Decision.jsx";
import { Working } from "./Working.jsx";
const H_OUT = { ok: ["ok", "✓"], fail: ["fail", "✗"], attn: ["attn", "!"] };
function hBody(r, width, i) {
  if (r.diff) {
    const bg = r.diff === "add" ? "var(--slab-diff-add-bg)" : r.diff === "del" ? "var(--slab-diff-del-bg)" : undefined;
    const fg = r.diff === "add" ? "var(--slab-diff-add-fg)" : r.diff === "del" ? "var(--slab-diff-del-fg)" : "var(--h-dim)";
    const sign = r.diff === "add" ? "+" : r.diff === "del" ? "−" : " ";
    return <Band key={i} bg={bg || "var(--h-block)"} width={width} left={[["faint", String(r.line).padStart(3, " ") + "  "], [fg, sign + " " + r.text]]} />;
  }
  return <Band key={i} bg="var(--h-block)" width={width} left={Array.isArray(r) ? r : [["dim", r]]} right={r.right || []} />;
}
export function Block({ name, arg, argTone = "ink", status, outcome = "", running = false, body = [], meta, decision, width = 76 }) {
  const glyph = running ? ["live", "▸ "] : status === "attn" ? ["attn", "! "] : ["dim", "▸ "];
  const nm = name.length > 10 ? name.slice(0, 9) + "…" : name.padEnd(10, " ");
  const right = running ? [] : [...(status && H_OUT[status] ? [[H_OUT[status][0], H_OUT[status][1]]] : []), ["dim", (status ? " " : "") + outcome]];
  return (
    <div>
      <div style={{ position: "relative" }}>
        <Band bg="var(--h-block-plus)" width={width} left={[glyph, ["dim", nm], [argTone, arg]]} right={running ? [["ink", "   "]] : right} />
        {running && <span style={{ position: "absolute", right: "2ch", top: 0 }}><Working /></span>}
      </div>
      {body.map((r, i) => hBody(r, width, i))}
      {decision ? <Decision width={width} {...decision} /> : meta ? <Band bg="var(--h-block)" width={width} left={meta.left || [["faint", meta]]} right={meta.right || []} /> : null}
    </div>
  );
}
