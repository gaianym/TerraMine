/**
 * Pools (add / replace / set primary + fallback) and timezone (plan PR F):
 * pure helpers for the dialog. The backend resolves the operation against
 * each miner's own pool list and re-checks everything; this only gives the
 * operator an honest per-miner plan before sending.
 */
import type { Settings } from "./alertBanner";
import { buildSettingsRequest } from "./power";
import type { FieldSupport, PoolGroup, PoolOp, PoolSpec, SettingsJobRequest, SettingsSupportRow } from "./power";

export type PoolPlan = { change: Settings; errors: string[]; rows: Array<{ ip: string; included: boolean; note: string }> };


export type PoolInput = {
  mode: "add" | "replace" | "set" | "none";
  /** 1-based position for replace. */
  position: string;
  url: string;
  user: string;
  password: string;
  /** Second pool for "set" (primary + fallback). */
  url2: string;
  user2: string;
  password2: string;
  timezone: string;
};

export const emptyPoolInput = (): PoolInput => ({
  mode: "none", position: "1", url: "", user: "", password: "", url2: "", user2: "", password2: "", timezone: "",
});

/** stratum+tcp://host:port with an explicit port (asic-rs would default to 80). */
export function poolUrlError(url: string): string | null {
  const t = url.trim();
  if (!t) return "Pool URL is empty";
  if (/\s/.test(t)) return "Pool URL contains spaces";
  const rest = t.includes("://") ? t.split("://")[1] : t;
  const auth = rest.split("/")[0];
  const m = auth.match(/^(.+):(\d+)$/);
  if (!m || Number(m[2]) < 1 || Number(m[2]) > 65535) return `Pool URL ${t} needs an explicit port (stratum+tcp://host:port)`;
  return null;
}

function pool(url: string, user: string, password: string, errors: string[]): PoolSpec | null {
  const e = poolUrlError(url);
  if (e) errors.push(e);
  if (!user.trim()) errors.push("Worker / user is empty");
  if (e || !user.trim()) return null;
  const p: PoolSpec = { url: url.trim(), user: user.trim() };
  if (password) p.password = password;
  return p;
}

export function buildPoolChange(input: PoolInput): { change: Settings; errors: string[] } {
  const errors: string[] = [];
  const change: Settings = {};
  if (input.mode === "add" || input.mode === "replace") {
    const p = pool(input.url, input.user, input.password, errors);
    if (input.mode === "add" && p) change.pool_op = { op: "add", pool: p };
    if (input.mode === "replace") {
      const pos = Number(input.position);
      if (!/^\d+$/.test(input.position.trim()) || pos < 1) errors.push("Position must be 1 or more");
      else if (p) change.pool_op = { op: "replace", index: pos - 1, pool: p };
    }
  } else if (input.mode === "set") {
    const a = pool(input.url, input.user, input.password, errors);
    const pools: PoolSpec[] = a ? [a] : [];
    if (input.url2.trim() || input.user2.trim()) {
      const b = pool(input.url2, input.user2, input.password2, errors);
      if (b) pools.push(b);
    }
    if (!errors.length) change.pool_op = { op: "set", groups: [{ name: "default", quota: 1, pools }] satisfies PoolGroup[] };
  }
  const tz = input.timezone.trim();
  if (tz) {
    if (!/^[A-Za-z_]+(\/[A-Za-z0-9_+-]+)*$/.test(tz)) errors.push(`${tz} is not an IANA timezone (e.g. America/Chicago)`);
    else change.timezone = tz;
  }
  if (change.pool_op && change.timezone) errors.push("Send a pool change on its own (timezone separately)");
  if (!change.pool_op && !change.timezone && !errors.length) errors.push("Choose a pool change or a timezone");
  return { change, errors };
}

const count = (g: PoolGroup[] | null | undefined) => (g ?? []).reduce((n, x) => n + x.pools.length, 0);

export function planPools(input: PoolInput, rows: SettingsSupportRow[], live: boolean): PoolPlan {
  const { change, errors } = buildPoolChange(input);
  const out = rows.map((r) => {
    if (!r.found) return { ip: r.ip, included: false, note: "not reachable or not recognised" };
    let sup: FieldSupport | undefined;
    if (change.pool_op) {
      sup = r.pools;
      if (!sup?.path) return { ip: r.ip, included: false, note: `pools not supported; tried: ${(sup?.tried ?? []).join("; ")}` };
      const have = count(r.current_pools);
      const max = r.pools_max ?? 0;
      const op: PoolOp = change.pool_op;
      const after = op.op === "add" ? have + 1 : op.op === "set" ? count(op.groups) : have;
      if (after > max) return { ip: r.ip, included: false, note: `${after} pools; this miner keeps at most ${max}` };
      if (op.op === "replace" && op.index >= have) return { ip: r.ip, included: false, note: `no pool at position ${op.index + 1} (miner has ${have})` };
      if (r.class === "axe_os" && after !== 2) return { ip: r.ip, included: false, note: "AxeOS always has a primary and a fallback pool (send two, or replace one)" };
    } else if (change.timezone) {
      sup = r.timezone;
      if (!sup?.path) return { ip: r.ip, included: false, note: `timezone not supported; tried: ${(sup?.tried ?? []).join("; ")}` };
      if (!r.timezone_allowed) return { ip: r.ip, included: false, note: `timezone not allowed on ${r.class ?? "this class"}` };
    }
    if (!sup?.path) return { ip: r.ip, included: false, note: "nothing to change" };
    const via = `via ${sup.path}`;
    if (!sup.confirmed && live) return { ip: r.ip, included: false, note: `${via}: dry run only (not yet confirmed on real hardware)` };
    return { ip: r.ip, included: true, note: `${via}${sup.confirmed ? "" : " (dry run only until hardware-confirmed)"}` };
  });
  return { change, errors, rows: out };
}

/** "pool.example.com:3333 acct.worker1, …" for a row's current pools. */
export function describePools(g: PoolGroup[] | null | undefined): string {
  const list = (g ?? []).flatMap((x) => x.pools).map((p) => `${p.url.replace(/^stratum\+tcp:\/\//, "")} ${p.user}`);
  return list.length ? list.join(", ") : "none";
}

/** Exact text the operator types for a live pool change. */
export const POOL_CONFIRM_TEXT = "change_pools";

export function buildPoolRequest(
  plan: PoolPlan,
  opts: { dryRun: boolean; typed: string; canarySize: number },
): SettingsJobRequest | null {
  const req = buildSettingsRequest(plan, { dryRun: opts.dryRun, concurrency: 4, staggerMs: 1000, canarySize: opts.canarySize, haltThreshold: 0.2 });
  if (!req) return null;
  if (req.change.pool_op && !opts.dryRun) {
    if (opts.typed.trim() !== POOL_CONFIRM_TEXT) return null;
    req.confirm = { allow_pool_change: true };
  }
  return req;
}
