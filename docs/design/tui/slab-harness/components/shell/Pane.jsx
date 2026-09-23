import React from "react";
import { Band } from "../cell/Band.jsx";
export function Pane({ sections = [], width = 38 }) {
  const inner = width - 8;
  return (
    <div style={{ background: "var(--h-block)", width: width + "ch", height: "100%", paddingTop: "1lh", boxSizing: "border-box" }}>
      {sections.map((s, i) => (
        <div key={i} style={{ marginBottom: "1lh" }}>
          <Band width={width} pad={4} left={[["dim", s.title]]} />
          {s.rows.map((r, j) => <Band key={j} width={width} pad={4} left={[[r[3] === "faint" ? "faint" : "dim", r[0]]]} right={[[r[2] || "ink", r[1]]]} />)}
        </div>
      ))}
    </div>
  );
}
