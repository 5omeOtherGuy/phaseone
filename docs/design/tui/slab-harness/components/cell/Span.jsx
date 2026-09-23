import React from "react";

const H_TONES = { ink: "var(--h-ink)", fg: "var(--h-ink)", dim: "var(--h-dim)", faint: "var(--h-faint)", attn: "var(--slab-attn)", fail: "var(--slab-fail)", ok: "var(--slab-ok)", live: "var(--slab-live)", ref: "var(--slab-ref)", syntax: "var(--slab-syntax)", ground: "var(--h-ground)" };
export function Span({ tone = "ink", bold, inverse, bg, children }) {
  const c = H_TONES[tone] || tone;
  return <span style={{ color: inverse ? "var(--h-ground)" : c, background: inverse ? c : bg, fontWeight: bold ? 700 : undefined }}>{children}</span>;
}
