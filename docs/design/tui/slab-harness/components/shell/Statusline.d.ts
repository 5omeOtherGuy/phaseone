/** Last row of the screen: host state only. Route chip neutral-inverted (never amber). Unknown spend renders —. Segments separated by 3 spaces, never a bar. */
export interface StatuslineProps { route?: string; repo?: string; branch?: string; effort?: string; ctx?: string; spend?: string; clock?: string; added?: number; removed?: number; width?: number; }
export function Statusline(props: StatuslineProps): JSX.Element;
