/**
 * Tool event — the three-band block. A: header on BLOCK+ (glyph · 10-col name · argument · right-aligned outcome).
 * B: body rows on BLOCK (verbatim, DIM; diff rows tinted). C: meta row, or a Decision on BLOCK+ when blocking.
 * @startingPoint section="Transcript" subtitle="Tool call block with body + decision" viewport="700x260"
 */
export type BodyRow = string | Array<[string, string, object?]> | { diff: "add" | "del" | "ctx"; line: number; text: string };
export interface BlockProps {
  name: string; arg: string; argTone?: "ink" | "ref";
  status?: "ok" | "fail" | "attn"; outcome?: string; running?: boolean;
  body?: BodyRow[]; meta?: string | { left: any[]; right?: any[] };
  decision?: { options: Array<{ key: string; label: string; disabled?: boolean; reason?: string }>; secondary?: string[] };
  width?: number;
}
export function Block(props: BlockProps): JSX.Element;
