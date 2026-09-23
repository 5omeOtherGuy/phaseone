Every tool call renders as a Block; nothing else in the transcript gets a background.
```jsx
<Block name="shell" arg="cargo test retry" status="fail" outcome="11.4s · exit 101" body={["assertion `left == right` failed"]} meta="· 86 more lines folded → [h-2b14]" />
<Block name="edit" arg="src/run.rs" argTone="ref" status="attn" outcome="+1 −1 · 1 of 1 files" body={[{diff:"del",line:88,text:"let ctx = …"}]} decision={{options:[{key:"y",label:"allow once"},{key:"n",label:"deny"}]}} />
```
- `running` swaps the outcome for the Working indicator and turns ▸ cyan.
- Outcome: ✓ green / ✗ red / ! amber glyph, facts DIM.
