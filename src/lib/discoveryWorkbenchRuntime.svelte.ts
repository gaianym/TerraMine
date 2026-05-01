import type { DiscoveryProgress, DiscoveryRowEvent } from "$lib/discoveryDefaults";

export function emptyWorkbenchProgress(): DiscoveryProgress {
  return {
    totalTargets: 0,
    probed: 0,
    alive: 0,
    missed: 0,
    enriched: 0,
    partial: 0,
    cancelled: 0,
  };
}

export type DiscoveryWorkbenchState = {
  rows: Map<string, DiscoveryRowEvent>;
  progress: DiscoveryProgress;
  missStreakByRow: Map<string, number>;
  movementByCell: Map<string, { direction: "up" | "down"; deltaText: string }>;
};

/**
 * Holds Results grid data and probe progress outside the route component so navigating
 * to Settings does not wipe in-memory discovery state on remount.
 * Mutate fields (e.g. `discoveryWorkbench.rows = next`) rather than replacing the export.
 */
export const discoveryWorkbench = $state<DiscoveryWorkbenchState>({
  rows: new Map(),
  progress: emptyWorkbenchProgress(),
  missStreakByRow: new Map(),
  movementByCell: new Map(),
});

/** Coalescing buffer — plain Map cleared in place, no whole-map replacement. */
export const workbenchPendingRows = new Map<string, DiscoveryRowEvent>();
