import React from "react";
import { Band } from "../cell/Band.jsx";
export function Picker({ question, mode = "pick one", options = [], focused = 0, note, width = 76 }) {
  return (
    <div>
      <Band bg="var(--h-block-plus)" width={width} left={[["attn", "! "], ["dim", "ask       "], ["ink", question]]} right={[["dim", mode]]} />
      {options.map((o, i) => i === focused
        ? <Band key={i} bg="var(--h-focus-row-bg)" width={width} left={[["ground", "▸ " + o.label.padEnd(22, " ")], ["ground", o.description || ""]]} />
        : <Band key={i} bg="var(--h-block)" width={width} left={[["faint", "· "], ["ink", o.label.padEnd(22, " ")], ["dim", o.description || ""]]} />)}
      <Band bg="var(--h-block)" width={width} left={[["dim", note || ""]]} right={[["faint", "space toggle   ⏎ confirm   esc dismiss"]]} />
    </div>
  );
}
