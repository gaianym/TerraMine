/**
 * Default options for every miner action confirm dialog (per-row and bulk).
 * Safety rule: dry run is ON by default; the operator must untick it to send.
 */
export const DEFAULT_ACTION_OPTIONS = Object.freeze({
  dryRun: true,
  verify: true,
  staggerMs: 1000,
  concurrency: 4,
});

export type ActionOptions = { dryRun: boolean; verify: boolean; staggerMs: number; concurrency: number };

export function defaultActionOptions(): ActionOptions {
  return { ...DEFAULT_ACTION_OPTIONS };
}
