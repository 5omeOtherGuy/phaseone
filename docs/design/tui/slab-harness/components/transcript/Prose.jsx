import React from "react";
import { Band } from "../cell/Band.jsx";
export function Prose({ lines = [], width = 76 }) {
  return <div>{lines.map((l, i) => <Band key={i} width={width} left={typeof l === "string" ? [["ink", l]] : l} />)}</div>;
}
