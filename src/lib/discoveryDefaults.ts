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

export function cloneDiscoverySettings(s: DiscoverySettings): DiscoverySettings {
  return { ...s };
}

/** Integer rounds + mins applied when persisting (mirrors UIntField mins on blur). */
export function finalizeDiscoverySettingsForSave(s: DiscoverySettings): DiscoverySettings {
  const r = Math.round;
  const m = Math.max;
  return cloneDiscoverySettings({
    ...s,
    probeTimeoutSec: m(1, r(Number(s.probeTimeoutSec) || 0)),
    probeConcurrency: m(1, r(Number(s.probeConcurrency) || 0)),
    enrichTimeoutSec: m(1, r(Number(s.enrichTimeoutSec) || 0)),
    enrichConcurrency: m(1, r(Number(s.enrichConcurrency) || 0)),
    probeRateLimitPerSec: m(0, r(Number(s.probeRateLimitPerSec) || 0)),
    enrichRateLimitPerSec: m(0, r(Number(s.enrichRateLimitPerSec) || 0)),
    autoDiscoverIntervalSec: m(5, r(Number(s.autoDiscoverIntervalSec) || 0)),
    autoEnrichIntervalSec: m(5, r(Number(s.autoEnrichIntervalSec) || 0)),
    retryJitterMs: m(0, r(Number(s.retryJitterMs) || 0)),
    probeQueueCap: m(1, r(Number(s.probeQueueCap) || 0)),
    enrichQueueCap: m(1, r(Number(s.enrichQueueCap) || 0)),
    uiCoalesceMs: m(0, r(Number(s.uiCoalesceMs) || 0)),
  });
}

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
