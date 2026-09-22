/** Composer — the one non-tool block: input row on BLOCK+ (amber › + caret), hint row on BLOCK. Lives in the transcript column. */
export interface ComposerProps { value?: string; placeholder?: string; hint?: string; secondary?: string; width?: number; onChange?: (v: string) => void; onSubmit?: (v: string) => void; inputRef?: any; }
export function Composer(props: ComposerProps): JSX.Element;
