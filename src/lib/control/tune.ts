/**
 * Frequency / core voltage / fan / target temperature (plan PR H): pure
 * helpers for the Tune dialog. Everything goes through the backend safety
 * pipeline (`run_settings_job`); each miner's plan comes back as a dry-run
 * row (`safety:row`), including "not supported; tried …" and the Bitaxe
 * power stand-in.
 *
 * Fan / target temperature apply fleetwide (plan PR E): AxeOS over its HTTP
 * API, other firmware through asic-rs fan config where it reports support.
 * Thermal thresholds are read only in asic-rs 0.8.6.
 */
import type { Settings } from "./alertBanner";

export type FanMode = "keep" | "manual" | "auto";

export type TuneInput = {
  frequencyMhz: string;
  coreVoltageMv: string;
  powerW: string;
  fanMode: FanMode;
  fanPct: string;
  tempTargetC: string;
};

export const EMPTY_TUNE: TuneInput = { frequencyMhz: "", coreVoltageMv: "", powerW: "", fanMode: "keep", fanPct: "", tempTargetC: "" };

export type TuneJobRequest = {
  ips: string[];
  change: Settings;
  dry_run: boolean;
  confirm: { allow_frequency_raise: boolean; force_low_fan: boolean };
  concurrency: number;
  stagger_ms: number;
  canary_size: number;
  halt_threshold: number;
};

function whole(raw: string, name: string, errors: string[], max?: number): number | undefined {
  const t = raw.trim();
  if (!t) return undefined;
  if (!/^\d+$/.test(t)) {
    errors.push(`${name} must be a whole number`);
    return undefined;
  }
  const n = Number(t);
  if (max !== undefined && n > max) errors.push(`${name} cannot exceed ${max}`);
  return n;
}

/** Typed values → settings change. Limits are enforced per miner by the backend. */
export function buildTuneChange(i: TuneInput): { change: Settings; errors: string[] } {
  const errors: string[] = [];
  const change: Settings = {};
  const f = whole(i.frequencyMhz, "Frequency", errors);
  const v = whole(i.coreVoltageMv, "Core voltage", errors);
  const p = whole(i.powerW, "Power", errors);
  if (f !== undefined) change.frequency_mhz = f;
  if (v !== undefined) change.core_voltage_mv = v;
  if (p !== undefined) change.power_limit_w = p;
  if (f !== undefined && p !== undefined) errors.push("Set either a frequency or a power target, not both");
  if (i.fanMode === "manual") {
    const pct = whole(i.fanPct, "Fan speed", errors, 100);
    if (pct === undefined && !i.fanPct.trim()) errors.push("Enter a fan speed for manual fan");
    change.auto_fan = false;
    if (pct !== undefined) change.fan_pct = pct;
  } else if (i.fanMode === "auto") {
    change.auto_fan = true;
    const t = whole(i.tempTargetC, "Target temperature", errors);
    if (t !== undefined) change.temp_target_c = t;
  }
  if (!Object.keys(change).length && !errors.length) errors.push("Nothing to change");
  return { change, errors };
}

/** Frequency / voltage changes restart AxeOS miners to apply. */
export function restartsAxeOs(change: Settings): boolean {
  return change.frequency_mhz != null || change.core_voltage_mv != null || change.power_limit_w != null;
}

/** Mirrors the backend `LOW_FAN_PCT`: manual fan below this needs an explicit OK. */
export const LOW_FAN_PCT = 30;

export function isLowFan(change: Settings): boolean {
  return change.auto_fan !== true && change.fan_pct != null && change.fan_pct < LOW_FAN_PCT;
}

export function buildTuneRequest(
  ips: string[],
  change: Settings,
  opts: { dryRun: boolean; allowRaise: boolean; canarySize: number; forceLowFan?: boolean },
): TuneJobRequest {
  return {
    ips,
    change,
    dry_run: opts.dryRun,
    confirm: { allow_frequency_raise: opts.allowRaise, force_low_fan: opts.forceLowFan ?? false },
    concurrency: 4,
    stagger_ms: 1000,
    canary_size: Math.max(1, Math.min(opts.canarySize, ips.length || 1)),
    halt_threshold: 0.2,
  };
}
