/** Batch settings-job alert (backend `safety:alert`), plus pure helpers for the banner. */

export const EVENT_SAFETY_ALERT = "safety:alert";

export type Settings = {
  frequency_mhz?: number | null;
  core_voltage_mv?: number | null;
  fan_pct?: number | null;
  auto_fan?: boolean | null;
  power_limit_w?: number | null;
  tuning_pct?: number | null;
};

export type FailureCause =
  | "overheat"
  | "unreachable"
  | "hashrate_drop"
  | "read_back_mismatch"
  | "write_failed"
  | "rollback_failed";

export type HealthSample = { reachable: boolean; hashrate_ghs: number | null; temp_c: number | null };

export type AlertMiner = {
  ip: string;
  status: string;
  before: Settings | null;
  attempted: Settings | null;
  after: Settings | null;
  sample: HealthSample | null;
  trigger: FailureCause | null;
  detail: string | null;
  likely_external: boolean;
};

export type AlertGroup = { cause: FailureCause; label: string; count: number; miners: AlertMiner[] };

export type RetryRequest = { ips: string[]; change: Settings; dry_run: boolean };

export type JobAlert = {
  run_id: string;
  total: number;
  rolled_back: number;
  rollback_failed: number;
  halted: number;
  headline: string;
  likely_external: boolean;
  external_note: string | null;
  groups: AlertGroup[];
  halted_ips: string[];
  retry: RetryRequest;
};

const FIELDS: Array<[keyof Settings, string]> = [
  ["frequency_mhz", "MHz"],
  ["core_voltage_mv", "mV"],
  ["fan_pct", "% fan"],
  ["power_limit_w", "W"],
  ["tuning_pct", "% tuning"],
];

function fieldValue(s: Settings | null, k: keyof Settings): string {
  const v = s?.[k];
  return v === null || v === undefined ? "?" : String(v);
}

/** "525 → 600 → 525 MHz" (before → attempted → after) for each changed field. */
export function describeChange(m: Pick<AlertMiner, "before" | "attempted" | "after">): string {
  const parts: string[] = [];
  for (const [k, unit] of FIELDS) {
    if (m.attempted?.[k] === null || m.attempted?.[k] === undefined) continue;
    parts.push(`${fieldValue(m.before, k)} → ${fieldValue(m.attempted, k)} → ${fieldValue(m.after, k)} ${unit}`);
  }
  if (m.attempted?.auto_fan !== null && m.attempted?.auto_fan !== undefined) {
    parts.push(`auto fan ${fieldValue(m.before, "auto_fan")} → ${m.attempted.auto_fan} → ${fieldValue(m.after, "auto_fan")}`);
  }
  return parts.join(", ");
}

/** Health at the time of failure, e.g. "95 °C, 0 GH/s" or "unreachable". */
export function describeSample(s: HealthSample | null): string {
  if (!s) return "";
  if (!s.reachable) return "unreachable";
  const out: string[] = [];
  if (s.temp_c !== null) out.push(`${Math.round(s.temp_c * 10) / 10} °C`);
  if (s.hashrate_ghs !== null) out.push(`${Math.round(s.hashrate_ghs)} GH/s`);
  return out.join(", ");
}

export function alertTone(a: JobAlert): "danger" | "warn" {
  return a.rollback_failed > 0 ? "danger" : "warn";
}

/** Keep one alert per job (newest first). */
export function pushAlert(list: JobAlert[], a: JobAlert): JobAlert[] {
  return [a, ...list.filter((x) => x.run_id !== a.run_id)];
}

export function dismissAlert(list: JobAlert[], runId: string): JobAlert[] {
  return list.filter((x) => x.run_id !== runId);
}

export function retryLabel(a: JobAlert): string {
  const n = a.retry.ips.length;
  return `Retry ${n} miner${n === 1 ? "" : "s"}${a.retry.dry_run ? " (dry run)" : ""}`;
}
