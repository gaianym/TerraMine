/**
 * Admin actions (plan step G): hashboard detail, read logs, change
 * password, factory reset. Pure helpers for the dialog; the backend
 * re-checks everything (capability gate per make / model / firmware,
 * typed confirmation, one device per destructive send, dry run default).
 */
import type { ActionStatus } from "./types";

export type AdminAction = "hashboards" | "read_logs" | "change_password" | "factory_reset";

export const ADMIN_ACTIONS: readonly AdminAction[] = ["hashboards", "read_logs", "change_password", "factory_reset"];

export const ADMIN_LABEL: Record<AdminAction, string> = {
  hashboards: "Hashboards",
  read_logs: "Read logs",
  change_password: "Change password",
  factory_reset: "Factory reset",
};

export const EVENT_ADMIN_ROW = "admin:row";
export const EVENT_ADMIN_DONE = "admin:done";

export type Hashboard = {
  position: number;
  hashrate_ghs?: number | null;
  expected_hashrate_ghs?: number | null;
  board_temp_c?: number | null;
  inlet_chip_temp_c?: number | null;
  outlet_chip_temp_c?: number | null;
  working_chips?: number | null;
  expected_chips?: number | null;
  active?: boolean | null;
  serial?: string | null;
  voltage_v?: number | null;
  frequency_mhz?: number | null;
  chips_reported?: number;
};

export type PathSupport = { path: string | null; confirmed: boolean; tried: string[] };

export type AdminSupportRow = {
  ip: string;
  found: boolean;
  class: string | null;
  model: string | null;
  firmware: string | null;
  actions: Partial<Record<AdminAction, PathSupport>>;
  device_login: boolean;
};

export type AdminOutcome = {
  ip: string;
  action: AdminAction;
  status: ActionStatus;
  msg: string | null;
  route?: string;
  paths_tried?: string[];
  hashboards?: Hashboard[];
  logs?: string;
  logs_truncated?: boolean;
};

export type AdminRequest = {
  ips: string[];
  action: AdminAction;
  dry_run: boolean;
  confirm?: { typed: string; allow_irreversible: boolean };
  new_password?: string;
  concurrency: number;
  stagger_ms: number;
  timeout_sec: number;
};

export const isRead = (a: AdminAction): boolean => a === "hashboards" || a === "read_logs";
export const isDestructive = (a: AdminAction): boolean => !isRead(a);

/** Exact text typed to send a destructive action live (the backend checks it). */
export const confirmText = (a: AdminAction): string => a;

export const NOT_CONFIRMED_NOTE = "dry run only: path not yet confirmed on real hardware";

export type AdminPlan = {
  errors: string[];
  rows: Array<{ ip: string; included: boolean; note: string }>;
};

/** Per-miner plan: which rows the action applies to, and why not. */
export function planAdmin(action: AdminAction, support: AdminSupportRow[], live: boolean): AdminPlan {
  const rows = support.map((r) => {
    if (!r.found) return { ip: r.ip, included: false, note: "not reachable or not recognised" };
    const s = r.actions[action];
    if (!s?.path) return { ip: r.ip, included: false, note: `not supported; tried: ${(s?.tried ?? []).join("; ") || "no path"}` };
    if (isRead(action)) return { ip: r.ip, included: true, note: `via ${s.path}` };
    if (!s.confirmed) return { ip: r.ip, included: !live, note: `via ${s.path}; ${NOT_CONFIRMED_NOTE}` };
    return { ip: r.ip, included: true, note: `via ${s.path}` };
  });
  const errors: string[] = [];
  const n = rows.filter((r) => r.included).length;
  if (live && isDestructive(action) && n !== 1) {
    errors.push(n === 0 ? "No miner can take this action live" : `${ADMIN_LABEL[action]} is sent live to exactly one miner at a time (${n} selected)`);
  }
  return { errors, rows };
}

/** 1..64 printable characters, typed twice. */
export function passwordError(pw: string, again: string): string | null {
  if (!pw) return "Enter the new password";
  if ([...pw].length > 64) return "Password is longer than 64 characters";
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f]/.test(pw)) return "Password contains control characters";
  if (pw !== again) return "Passwords do not match";
  return null;
}

export function buildAdminRequest(
  action: AdminAction,
  plan: AdminPlan,
  opts: { dryRun: boolean; typed: string; acknowledged: boolean; password: string; passwordAgain: string },
): AdminRequest | null {
  const ips = plan.rows.filter((r) => r.included).map((r) => r.ip);
  if (!ips.length || plan.errors.length) return null;
  const read = isRead(action);
  const req: AdminRequest = {
    ips,
    action,
    dry_run: read ? false : opts.dryRun,
    concurrency: 4,
    stagger_ms: read ? 200 : 500,
    timeout_sec: 15,
  };
  if (action === "change_password" && !opts.dryRun) {
    if (passwordError(opts.password, opts.passwordAgain)) return null;
    req.new_password = opts.password;
  }
  if (!read && !opts.dryRun) {
    if (!opts.acknowledged || opts.typed.trim() !== confirmText(action)) return null;
    req.confirm = { typed: confirmText(action), allow_irreversible: true };
  }
  return req;
}

const rate = (g: number) => (g >= 1000 ? `${(g / 1000).toFixed(2)} TH/s` : `${g.toFixed(0)} GH/s`);

/** "63/63 chips · 4.61 TH/s · 72°C" for one board. */
export function describeBoard(b: Hashboard): string {
  const parts: string[] = [];
  if (b.working_chips != null) parts.push(b.expected_chips != null ? `${b.working_chips}/${b.expected_chips} chips` : `${b.working_chips} chips`);
  if (b.hashrate_ghs != null) parts.push(rate(b.hashrate_ghs));
  const t = b.outlet_chip_temp_c ?? b.inlet_chip_temp_c ?? b.board_temp_c;
  if (t != null) parts.push(`${t.toFixed(0)}°C`);
  if (b.voltage_v != null) parts.push(`${b.voltage_v.toFixed(2)} V`);
  if (b.frequency_mhz != null) parts.push(`${b.frequency_mhz.toFixed(0)} MHz`);
  return parts.join(" · ") || "no data";
}

export function boardDegraded(b: Hashboard): boolean {
  if (b.working_chips != null && b.expected_chips != null && b.working_chips < b.expected_chips) return true;
  if (b.active === false) return true;
  return b.hashrate_ghs != null && b.hashrate_ghs <= 0;
}
