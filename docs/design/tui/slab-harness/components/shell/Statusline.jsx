import React from "react";
import { Band } from "../cell/Band.jsx";
export function Statusline({ route = "Fable 5.1", repo = "harness", branch = "main", effort = "high", ctx = "6%", spend = "—", clock = "0h02", added = 0, removed = 0, width = 116 }) {
  return (
    <Band bg="var(--h-block-plus)" width={width} pad={1}
      left={[["var(--h-route-chip-bg)", " " + route + " ", { inverse: true }], ["ink", "   " + repo], ["dim", " " + branch], ["dim", "   effort "], ["ink", effort]]}
      right={[["dim", "ctx "], ["ink", ctx], ["dim", "   spend "], ["ink", spend], ["ink", "   " + clock], ["ok", "   +" + added], ["fail", " −" + removed]]} />
  );
}
