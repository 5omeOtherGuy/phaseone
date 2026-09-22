/** Run of cells with a role tone (ink · dim · faint · attn · fail · ok · live · ref). Vendored from SLAB Core cell/Span. */
export interface SpanProps { tone?: "ink" | "dim" | "faint" | "attn" | "fail" | "ok" | "live" | "ref" | "syntax" | string; bold?: boolean; inverse?: boolean; bg?: string; children: React.ReactNode; }
export function Span(props: SpanProps): JSX.Element;
