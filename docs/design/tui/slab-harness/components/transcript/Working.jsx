import React from "react";

export function Working() {
  return <span>{[0, 1, 2].map(i => <span key={i} style={{ color: "var(--h-running)", animation: "h-work 1.1s infinite", animationDelay: i * 0.18 + "s" }}>▪</span>)}</span>;
}
