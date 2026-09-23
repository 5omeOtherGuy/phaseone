/** ask event: question header, one row per option, focused row fully amber-inverted, hint row. */
export interface PickerProps { question: string; mode?: "pick one" | "pick any"; options: Array<{ label: string; description?: string }>; focused?: number; note?: string; width?: number; }
export function Picker(props: PickerProps): JSX.Element;
