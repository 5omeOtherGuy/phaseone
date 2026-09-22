import React from "react";
import { Band } from "../cell/Band.jsx";
export function OperatorInput({ text, width = 76 }) {
  return <Band width={width} left={[["attn", "› "], ["ink", text]]} />;
}
