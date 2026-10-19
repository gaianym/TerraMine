/**
 * Power limit + tuning percent (plan PR C): types for the backend
 * `settings_support` / `run_settings_job` commands and pure helpers for the
 * dialog (typed-value validation against each miner's limits).
 */
import type { Settings } from "./alertBanner";

export type Range = [number, number];

export type FieldSupport = {
  /** Backend that would take the write; null = not supported. */
  path: string | null;
  /** False = not yet confirmed on real hardware (dry run only). */
  confirmed: boolean;
  tried: string[];
};

export type SettingsSupportRow = {
  ip: string;
  found: boolean;
  class: string | null;
  model: string | null;
  power_w: Range | null;
  tuning_pct: Range | null;
  power: FieldSupport;
  tuning: FieldSupport;
  fan_pct?: [number, number] | null;
  temp_target_c?: [number, number] | null;
  fan?: FieldSupport;
  fan_auto?: FieldSupport;
  /** Pools / timezone (plan PR F). */
  pools?: FieldSupport;
  pools_max?: number | null;
  current_pools?: PoolGroup[] | null;
  timezone?: FieldSupport;
  timezone_allowed?: boolean;
  current_timezone?: string | null;
};

export type PoolSpec = { url: string; user: string; password?: string | null };
export type PoolGroup = { name: string; quota: number; pools: PoolSpec[] };
export type PoolOp =
  | { op: "set"; groups: PoolGroup[] }
  | { op: "add"; pool: PoolSpec }
  | { op: "replace"; index: number; pool: PoolSpec };

export type WriteOutcome = {
  ip: string;
  status: "dry_run" | "refused" | "no_path" | "failed" | "verified" | "rolled_back" | "rollback_failed" | "skipped";
  msg: string | null;
  paths_tried: string[];
};

export type SettingsJobRequest = {
  ips: string[];
  change: Settings;
  dry_run: boolean;
  confirm?: { allow_pool_change?: boolean };
  concurrency: number;
  stagger_ms: number;
  canary_size?: number;
  halt_threshold: number;
};

export const EVENT_SAFETY_ROW = "safety:row";
export const EVENT_SAFETY_DONE = "safety:done";

export type PowerInput = { powerW: string; tuningPct: string };

export type PowerPlanRow = {
  ip: string;
  /** Will be sent (or dry-run) with this change. */
  included: boolean;
  /** Why not included / what is wrong, else a short "via ..." note. */
  note: string;
};

export type PowerPlan = { change: Settings; errors: string[]; rows: PowerPlanRow[] };

function parseField(raw: string, name: string, errors: string[]): number | undefined {
  const t = raw.trim();
  if (!t) return undefined;
  if (!/^\d+$/.test(t)) {
    errors.push(`${name} must be a whole number`);
    return undefined;
  }
  return Number(t);
}

export function formatRange(r: Range | null, unit: string): string {
  return r ? `${r[0]}–${r[1]} ${unit}` : "no limit set (write refused)";
}

/**
 * Validate typed values and decide, per miner, whether the change can be
 * sent. Mirrors the backend order: path first, then limits. `live` = dry
 * run off; paths not confirmed on hardware are left out of a live send.
 */
export function planPower(input: PowerInput, rows: SettingsSupportRow[], live: boolean): PowerPlan {
  const errors: string[] = [];
  const power = parseField(input.powerW, "Power limit", errors);
  const tuning = parseField(input.tuningPct, "Tuning percent", errors);
  if (tuning !== undefined && tuning > 100) errors.push("Tuning percent cannot exceed 100");
  const change: Settings = {};
  if (power !== undefined) change.power_limit_w = power;
  if (tuning !== undefined) change.tuning_pct = tuning;
  if (power === undefined && tuning === undefined && !errors.length) errors.push("Enter a power limit and/or a tuning percent");
  const out: PowerPlanRow[] = rows.map((r) => {
    if (!r.found) return { ip: r.ip, included: false, note: "not reachable or not recognised" };
    const fields: Array<[number | undefined, FieldSupport, Range | null, string, string]> = [
      [power, r.power, r.power_w, "power limit", "W"],
      [tuning, r.tuning, r.tuning_pct, "tuning percent", "%"],
    ];
    const paths = new Set<string>();
    let unconfirmed = false;
    for (const [v, sup, range, label, unit] of fields) {
      if (v === undefined) continue;
      if (!sup.path) return { ip: r.ip, included: false, note: `${label} not supported; tried: ${sup.tried.join("; ")}` };
      if (!range) return { ip: r.ip, included: false, note: `no ${label} limits for ${r.model ?? "this model"}` };
      if (v < range[0] || v > range[1]) {
        return { ip: r.ip, included: false, note: `${v} ${unit} outside ${formatRange(range, unit)}` };
      }
      paths.add(sup.path);
      unconfirmed ||= !sup.confirmed;
    }
    if (!paths.size) return { ip: r.ip, included: false, note: "nothing to change" };
    const via = `via ${[...paths].join(", ")}`;
    if (unconfirmed && live) return { ip: r.ip, included: false, note: `${via}: dry run only (not yet confirmed on real hardware)` };
    return { ip: r.ip, included: true, note: unconfirmed ? `${via} (dry run only until hardware-confirmed)` : via };
  });
  return { change, errors, rows: out };
}

export function buildSettingsRequest(
  plan: PowerPlan,
  opts: { dryRun: boolean; concurrency: number; staggerMs: number; canarySize: number; haltThreshold: number },
): SettingsJobRequest | null {
  const ips = plan.rows.filter((r) => r.included).map((r) => r.ip);
  if (plan.errors.length || !ips.length) return null;
  return {
    ips,
    change: plan.change,
    dry_run: opts.dryRun,
    concurrency: opts.concurrency,
    stagger_ms: opts.staggerMs,
    canary_size: Math.max(1, Math.min(opts.canarySize, ips.length)),
    halt_threshold: opts.haltThreshold,
  };
}
