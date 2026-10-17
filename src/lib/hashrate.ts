/**
 * Hashrate parsing and display. One canonical set of unit strings:
 * H/s, kH/s, MH/s, GH/s, TH/s, PH/s, EH/s, ZH/s.
 *
 * Unit spellings from miners/firmware are normalised first, including
 * enum names ("GigaHash"), suffix-less ("GH", "ghs"), and the common
 * frequency typo "GHz" / "GHz/s", so the grid never shows e.g. "GHz/s".
 */
export const HASH_RATE_UNIT_TIERS = ["H/s", "kH/s", "MH/s", "GH/s", "TH/s", "PH/s", "EH/s", "ZH/s"] as const;
export type HashRateUnit = (typeof HASH_RATE_UNIT_TIERS)[number];

const PREFIX_TIER: Record<string, number> = {
  "": 0,
  k: 1, kilo: 1,
  m: 2, mega: 2,
  g: 3, giga: 3,
  t: 4, tera: 4,
  p: 5, peta: 5,
  e: 6, exa: 6,
  z: 7, zetta: 7,
};

/** Canonical unit for a raw unit string, or null if it is not a hashrate unit. */
export function normalizeHashRateUnit(raw: unknown): HashRateUnit | null {
  if (typeof raw !== "string") return null;
  let k = raw.trim().toLowerCase().replace(/[\s_]+/g, "");
  if (!k) return null;
  k = k.replace(/(\/s|\/sec|\/second|ps|persecond)$/, "");
  const m = k.match(/^(kilo|mega|giga|tera|peta|exa|zetta|k|m|g|t|p|e|z)?(hz|hashes|hash|hs|h)$/);
  if (!m) return null;
  const tier = PREFIX_TIER[m[1] ?? ""];
  return tier === undefined ? null : HASH_RATE_UNIT_TIERS[tier];
}

function tierIndex(unit: HashRateUnit): number {
  return HASH_RATE_UNIT_TIERS.indexOf(unit);
}

/** Scale an amount in `unit` to the tier where 1 <= amount < 1000. */
export function scaleHashRate(amount: number, unit: HashRateUnit): { amount: number; unit: HashRateUnit } {
  let idx = tierIndex(unit);
  if (!(amount > 0)) return { amount: 0, unit: "H/s" };
  while (amount >= 1000 && idx < HASH_RATE_UNIT_TIERS.length - 1) {
    amount /= 1000;
    idx += 1;
  }
  while (amount < 1 && idx > 0) {
    amount *= 1000;
    idx -= 1;
  }
  return { amount, unit: HASH_RATE_UNIT_TIERS[idx] };
}

/** Split a hashrate cell value (number in H/s, "1.2 TH/s", or {value, unit}) into amount + unit. */
function splitHashRate(value: unknown): { amount: number; unit: HashRateUnit } | null {
  if (value === undefined || value === null) return null;
  if (typeof value === "number") return Number.isFinite(value) ? { amount: value, unit: "H/s" } : null;
  if (typeof value === "string") {
    const m = value.trim().match(/^(-?\d+(?:\.\d+)?)\s*(.*)$/);
    if (!m) return null;
    const amount = Number.parseFloat(m[1]);
    if (!Number.isFinite(amount)) return null;
    if (!m[2]) return { amount, unit: "H/s" };
    const unit = normalizeHashRateUnit(m[2]);
    return unit ? { amount, unit } : null;
  }
  if (typeof value === "object") {
    const rec = value as Record<string, unknown>;
    const raw = rec.value;
    const amount = typeof raw === "number" ? raw : typeof raw === "string" ? Number.parseFloat(raw) : Number.NaN;
    if (!Number.isFinite(amount)) return null;
    if (rec.unit === undefined || rec.unit === null || rec.unit === "") return { amount, unit: "H/s" };
    const unit = normalizeHashRateUnit(rec.unit);
    return unit ? { amount, unit } : null;
  }
  return null;
}

/** Hashrate in H/s, or null if the value/unit is not understood. */
export function parseHashRateToHs(value: unknown): number | null {
  const s = splitHashRate(value);
  return s ? s.amount * 1000 ** tierIndex(s.unit) : null;
}

/** Display tier for a cell value (what formatHashRateValue prints). */
export function getNormalizedHashRateDisplay(value: unknown): { amount: number; unit: HashRateUnit } | null {
  const s = splitHashRate(value);
  return s ? scaleHashRate(Math.max(0, s.amount), s.unit) : null;
}

/** "979.20 GH/s" etc. Unknown values render as an em dash. */
export function formatHashRateValue(value: unknown): string {
  const d = getNormalizedHashRateDisplay(value);
  return d ? `${d.amount.toFixed(2)} ${d.unit}` : "\u2014";
}
