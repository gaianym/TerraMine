/** Mirrors `DiscoverySettings` from Rust (camelCase). */
export type DiscoverySettings = {
  probeTimeoutSec: number;
  probeConcurrency: number;
  probeRateLimitPerSec: number;
  enrichTimeoutSec: number;
  enrichConcurrency: number;
  enrichRateLimitPerSec: number;
  autoDiscoverEnabled: boolean;
  autoDiscoverIntervalSec: number;
  autoEnrichEnabled: boolean;
  autoEnrichIntervalSec: number;
  retryJitterMs: number;
  probeQueueCap: number;
  enrichQueueCap: number;
  uiCoalesceMs: number;
};

export const defaultDiscoverySettings: DiscoverySettings = {
  probeTimeoutSec: 5,
  probeConcurrency: 48,
  probeRateLimitPerSec: 0,
  enrichTimeoutSec: 20,
  enrichConcurrency: 24,
  enrichRateLimitPerSec: 0,
  autoDiscoverEnabled: false,
  autoDiscoverIntervalSec: 60,
  autoEnrichEnabled: false,
  autoEnrichIntervalSec: 60,
  retryJitterMs: 250,
  probeQueueCap: 800,
  enrichQueueCap: 800,
  uiCoalesceMs: 48,
};

export type RowStatus =
  | "Queued"
  | "Probing"
  | "Alive"
  | "Enriching"
  | "Enriched"
  | "Partial"
  | "Miss"
  | "Cancelled";

export type DiscoveryRowEvent = {
  id: string;
  status: RowStatus;
  row?: Record<string, unknown>;
};

export type DiscoveryProgress = {
  totalTargets: number;
  probed: number;
  alive: number;
  missed: number;
  enriched: number;
  partial: number;
  cancelled: number;
};
