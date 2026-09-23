import React from "react";
import { Band } from "../cell/Band.jsx";
const H_WG = { review: ["attn", "! "], running: ["live", "▪ "], done: ["ok", "✓ "], queued: ["faint", "· "] };
export function Worker({ name, route, state = "running", elapsed = "", cost = "—", owns, activity, width = 76 }) {
  const g = H_WG[state];
  const q = state === "queued";
  return (
    <div>
      <Band bg="var(--h-block)" width={width} left={[g, [q ? "faint" : "ink", name.padEnd(16, " ")], ["dim", route]]} right={[["dim", elapsed + " · " + cost]]} />
      <Band bg="var(--h-block)" width={width} left={[["ink", "  "], ["dim", "owns  "], ["ref", owns]]} />
      {activity && <Band bg="var(--h-block)" width={width} left={[["ink", "  "], ["dim", "↳ "], [q ? "faint" : "ink", activity]]} />}
    </div>
  );
}
