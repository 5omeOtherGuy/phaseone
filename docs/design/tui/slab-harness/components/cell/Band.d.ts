/**
 * One cell row filled edge to edge at `width` columns: 2-col padding, left segments, right-aligned segments.
 * Left truncates with … before right ever does. Every other harness component is built from Bands.
 */
export type Seg = [tone: string, text: string, opts?: { bold?: boolean; inverse?: boolean; bg?: string }];
export interface BandProps { bg?: string; left?: Seg[]; right?: Seg[]; width?: number; pad?: number; minGap?: number; }
export function Band(props: BandProps): JSX.Element;
