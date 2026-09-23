import React from "react";
import { Band } from "../cell/Band.jsx";
import { Span } from "../cell/Span.jsx";
export function Composer({ value = "", placeholder = "message, / for commands", hint = "⏎ send   ⇧⏎ newline", secondary = "^O pane   ^C cancel", width = 76, onChange, onSubmit, inputRef }) {
  return (
    <div>
      <div style={{ whiteSpace: "pre", background: "var(--h-block-plus)", width: width + "ch", display: "flex" }}>
        <span>  </span><Span tone="attn">› </Span>
        <input ref={inputRef} value={value} placeholder={placeholder} onChange={e => onChange && onChange(e.target.value)} onKeyDown={e => e.key === "Enter" && onSubmit && onSubmit(value)}
          style={{ flex: 1, border: 0, outline: "none", background: "transparent", color: "var(--h-ink)", font: "inherit", padding: 0, caretColor: "var(--slab-attn)" }} />
        <span>  </span>
      </div>
      <Band bg="var(--h-block)" width={width} left={[["faint", hint]]} right={[["faint", secondary]]} />
    </div>
  );
}
