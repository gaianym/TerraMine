/**
 * Up/down change tickers in the grid. Every ticker carries its own metric's
 * unit; hashrate is auto-scaled to the value column's tier (H/s … ZH/s)
 * instead of raw hashes.
 */
export const HASH_RATE_UNIT_TIERS = ["H/s", "KH/s", "MH/s", "GH/s", "TH/s", "PH/s", "EH/s", "ZH/s"] as const;
export type HashRateUnit = (typeof HASH_RATE_UNIT_TIERS)[number];

/** Columns that get an up/down ticker, with the unit each one must show. */
export const TICKER_UNITS = {
  hashrate: "H/s (auto-scaled)",
  average_temperature: "\u00B0C",
  wattage: "W",
  efficiency: "J/TH",
  fans: "rpm",
  fan_percent: "%",
  pools_accepted: "shares",
  pools_rejected: "shares",
} as const;
export type TickerColumn = keyof typeof TICKER_UNITS;

function autoTier(absHs: number): { amount: number; unit: HashRateUnit } {
  let idx = 0;
  let amount = absHs;
  while (amount >= 1000 && idx < HASH_RATE_UNIT_TIERS.length - 1) {
    amount /= 1000;
    idx += 1;
  }
  return { amount, unit: HASH_RATE_UNIT_TIERS[idx] };
}

/**
 * Hashrate delta (input in H/s). Uses the value column's unit when given, so
 * "1.20 TH/s ↑ +0.05 TH/s" reads consistently; falls back to auto-scaling the
 * delta itself when the value has no tier (e.g. 0 H/s right after reboot) or
 * the delta would round to 0.00 in that tier.
 */
export function formatHashRateDelta(absDeltaHs: number, valueUnit?: string | null): string {
  if (!Number.isFinite(absDeltaHs)) return "\u2014";
  const abs = Math.abs(absDeltaHs);
  const idx = valueUnit ? HASH_RATE_UNIT_TIERS.indexOf(valueUnit as HashRateUnit) : -1;
  if (idx > 0) {
    const amount = abs / 1000 ** idx;
    if (amount >= 0.005) return `${amount.toFixed(2)} ${HASH_RATE_UNIT_TIERS[idx]}`;
  }
  const t = autoTier(abs);
  return `${t.amount.toFixed(2)} ${t.unit}`;
}

/** Ticker text (without arrow/sign) for a non-hashrate metric, always with its unit. */
export function formatMetricDelta(col: Exclude<TickerColumn, "hashrate">, absDelta: number): string {
  if (!Number.isFinite(absDelta)) return "\u2014";
  const a = Math.abs(absDelta);
  switch (col) {
    case "average_temperature":
      return `${a < 1 ? a.toFixed(1) : String(Math.round(a))} \u00B0C`;
    case "wattage":
      return `${a.toFixed(1)} W`;
    case "efficiency":
      return `${a.toFixed(2)} J/TH`;
    case "fans":
      return `${Math.round(a)} rpm`;
    case "fan_percent":
      return `${Math.round(a)}%`;
    case "pools_accepted":
    case "pools_rejected": {
      const n = Math.round(a);
      return `${n.toLocaleString("en-US")} ${n === 1 ? "share" : "shares"}`;
    }
  }
}
