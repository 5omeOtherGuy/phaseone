/** Delegate worker group inside a Block body: glyph+name+route / owns paths / ↳ activity. Indents once, never deeper. Unknown cost renders —. */
export interface WorkerProps { name: string; route: string; state?: "review" | "running" | "done" | "queued"; elapsed?: string; cost?: string; owns: string; activity?: string; width?: number; }
export function Worker(props: WorkerProps): JSX.Element;
