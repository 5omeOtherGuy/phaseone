/** Assistant prose on GROUND, column 3, INK. No gutter, no role label, no bubble. Lines are pre-wrapped strings or Seg arrays. */
export interface ProseProps { lines: Array<string | Array<[string, string]>>; width?: number; }
export function Prose(props: ProseProps): JSX.Element;
