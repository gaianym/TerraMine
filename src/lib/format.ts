export const EM_DASH = "\u2014";

/** Generic grid cell: missing / null values render as an em dash, never "null". */
export function formatGridValue(value: unknown): string {
  if (value === undefined || value === null) return EM_DASH;
  if (typeof value === "string") return value.trim().toLowerCase() === "null" ? EM_DASH : value;
  return JSON.stringify(value);
}

/** Efficiency in J/TH; null / non-numeric (e.g. 0 H/s right after reboot) renders as an em dash. */
export function formatEfficiency(value: unknown): string {
  if (typeof value === "number") return Number.isFinite(value) ? `${value.toFixed(2)} J/TH` : EM_DASH;
  if (typeof value === "string") {
    const parsed = Number.parseFloat(value.trim());
    return Number.isFinite(parsed) ? `${parsed.toFixed(2)} J/TH` : EM_DASH;
  }
  return EM_DASH;
}
