/**
 * Tuning target, presets and scaling (plan PR D): pure helpers for the
 * dialog. One target kind per request (mode, hashrate or preset), plus
 * optional scaling. Mirrors the backend order: path first, then limits.
 * Paths not confirmed on hardware are left out of a live send.
 */
import type { Settings } from "./alertBanner";
import { formatRange, type FieldSupport, type PowerPlan, type PowerPlanRow, type Range, type SettingsSupportRow, type TuningMode } from "./power";

export type TargetKind = "none" | "mode" | "hashrate" | "preset";

export type TargetInput = {
  kind: TargetKind;
  mode: TuningMode | "";
  /** Hashrate target in TH/s as typed (sent as GH/s). */
  hashrateThs: string;
  preset: string;
  scalingStep: string;
  scalingMin: string;
};

export const MODE_LABEL: Record<TuningMode, string> = { low: "Low", normal: "Normal", high: "High" };

function whole(raw: string, name: string, errors: string[]): number | undefined {
  const t = raw.trim();
  if (!t) return undefined;
  if (!/^\d+$/.test(t)) {
    errors.push(`${name} must be a whole number`);
    return undefined;
  }
  return Number(t);
}

/** TH/s as typed → whole GH/s (up to 3 decimals). */
export function thsToGhs(raw: string, errors: string[]): number | undefined {
  const t = raw.trim();
  if (!t) return undefined;
  if (!/^\d+(\.\d{1,3})?$/.test(t)) {
    errors.push("Hashrate target must be a number in TH/s (up to 3 decimals)");
    return undefined;
  }
  return Math.round(Number(t) * 1000);
}

export function buildTargetChange(input: TargetInput): { change: Settings; errors: string[] } {
  const errors: string[] = [];
  const change: Settings = {};
  if (input.kind === "mode") {
    if (!input.mode) errors.push("Pick a mining mode");
    else change.tuning_mode = input.mode;
  } else if (input.kind === "hashrate") {
    const g = thsToGhs(input.hashrateThs, errors);
    if (g === undefined && !errors.length) errors.push("Enter a hashrate target");
    if (g !== undefined) change.tuning_hashrate_ghs = g;
  } else if (input.kind === "preset") {
    const p = input.preset.trim();
    if (!p) errors.push("Pick a preset");
    else change.tuning_preset = p;
  }
  const step = whole(input.scalingStep, "Scaling step", errors);
  const min = whole(input.scalingMin, "Scaling minimum", errors);
  if ((step === undefined) !== (min === undefined)) errors.push("Scaling needs both a step and a minimum");
  else if (step !== undefined && min !== undefined) {
    change.scaling_step = step;
    change.scaling_min = min;
  }
  if (!Object.keys(change).length && !errors.length) errors.push("Choose a tuning target and/or scaling");
  return { change, errors };
}

const inRange = (v: number, r: Range | null | undefined) => !!r && v >= r[0] && v <= r[1];

export function planTarget(input: TargetInput, rows: SettingsSupportRow[], live: boolean): PowerPlan {
  const { change, errors } = buildTargetChange(input);
  const out: PowerPlanRow[] = rows.map((r) => {
    if (!r.found) return { ip: r.ip, included: false, note: "not reachable or not recognised" };
    const checks: Array<[FieldSupport | undefined, string, string | null]> = [];
    if (change.tuning_mode) {
      const ok = (r.tuning_modes ?? []).includes(change.tuning_mode);
      checks.push([r.mode, "mining mode", ok ? null : `mode ${MODE_LABEL[change.tuning_mode]} not allowed on ${r.model ?? "this model"}`]);
    }
    if (change.tuning_hashrate_ghs != null) {
      const v = change.tuning_hashrate_ghs;
      checks.push([r.hashrate, "hashrate target", inRange(v, r.tuning_hashrate_ghs) ? null : `${v} GH/s outside ${formatRange(r.tuning_hashrate_ghs ?? null, "GH/s")}`]);
    }
    if (change.tuning_preset) {
      const listed = (r.presets ?? []).includes(change.tuning_preset);
      checks.push([r.preset, "preset", listed ? null : `preset ${change.tuning_preset} not offered by this miner`]);
    }
    if (change.scaling_step != null && change.scaling_min != null) {
      const bad =
        !inRange(change.scaling_step, r.scaling_step) || !inRange(change.scaling_min, r.scaling_min)
          ? `scaling outside step ${formatRange(r.scaling_step ?? null, "")} / min ${formatRange(r.scaling_min ?? null, "")}`
          : null;
      checks.push([r.scaling, "scaling", bad]);
    }
    const paths = new Set<string>();
    let unconfirmed = false;
    for (const [sup, label, bad] of checks) {
      if (!sup?.path) return { ip: r.ip, included: false, note: `${label} not supported; tried: ${(sup?.tried ?? []).join("; ")}` };
      if (bad) return { ip: r.ip, included: false, note: bad };
      paths.add(sup.path);
      unconfirmed ||= !sup.confirmed;
    }
    if (!paths.size) return { ip: r.ip, included: false, note: "nothing to change" };
    const via = `via ${[...paths].join(", ")}`;
    const standIn = paths.has("axeos-http") && (change.tuning_mode || change.tuning_hashrate_ghs != null) ? " (frequency stand-in)" : "";
    if (unconfirmed && live) return { ip: r.ip, included: false, note: `${via}${standIn}: dry run only (not yet confirmed on real hardware)` };
    return { ip: r.ip, included: true, note: `${via}${standIn}${unconfirmed ? " (dry run only until hardware-confirmed)" : ""}` };
  });
  return { change, errors, rows: out };
}

/** Presets offered by every included miner (for the picker). */
export function commonPresets(rows: SettingsSupportRow[]): string[] {
  const lists = rows.filter((r) => r.found && r.preset?.path).map((r) => r.presets ?? []);
  if (!lists.length) return [];
  return lists.reduce((a, b) => a.filter((x) => b.includes(x)));
}
