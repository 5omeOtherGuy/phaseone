/** Right pane (concept): BLOCK fill, 4-col padding, label DIM at left, value right-aligned. Sections separated by one blank row. */
export interface PaneProps { sections: Array<{ title: string; rows: Array<[label: string, value: string, tone?: string, labelTone?: string]> }>; width?: number; }
export function Pane(props: PaneProps): JSX.Element;
