/** Blocking decision row on BLOCK+. Keys are amber-inverted 3-cell chips; labels INK; ungrantable options FAINT with reason. */
export interface DecisionOption { key: string; label: string; disabled?: boolean; reason?: string; }
export interface DecisionProps { options: DecisionOption[]; secondary?: string[]; width?: number; }
export function Decision(props: DecisionProps): JSX.Element;
