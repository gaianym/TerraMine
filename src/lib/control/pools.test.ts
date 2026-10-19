import { describe, expect, it } from "vitest";
import { describeChange } from "./alertBanner";
import type { SettingsSupportRow } from "./power";
import { POOL_CONFIRM_TEXT, buildPoolChange, buildPoolRequest, describePools, emptyPoolInput, planPools, poolUrlError } from "./pools";

const sup = (path: string | null, confirmed = false) => ({ path, confirmed, tried: path ? [path] : ["asic-rs: no pools", "s9-cgi: whole-config rewrite"] });
const POOL = { url: "stratum+tcp://pool.example.com:3333", user: "acct.worker1", password: "<redacted>" };
function row(ip: string, over: Partial<SettingsSupportRow> = {}): SettingsSupportRow {
  return {
    ip, found: true, class: "braiins_os", model: "S19", power_w: null, tuning_pct: null,
    power: sup(null), tuning: sup(null), pools: sup("asic-rs"), pools_max: 3,
    current_pools: [{ name: "default", quota: 1, pools: [POOL] }],
    timezone: sup("asic-rs"), timezone_allowed: true, current_timezone: "UTC", ...over,
  } as SettingsSupportRow;
}
const inp = (o: Partial<ReturnType<typeof emptyPoolInput>>) => ({ ...emptyPoolInput(), ...o });

describe("pool URL", () => {
  it("needs an explicit port (asic-rs would default to 80)", () => {
    expect(poolUrlError("stratum+tcp://pool.example.com:3333")).toBeNull();
    expect(poolUrlError("pool.example.com:3333")).toBeNull();
    expect(poolUrlError("stratum+tcp://pool.example.com")).toMatch(/explicit port/);
    expect(poolUrlError("pool.example.com:0")).toMatch(/explicit port/);
    expect(poolUrlError("pool .example.com:1")).toMatch(/spaces/);
  });
});

describe("buildPoolChange", () => {
  it("add / replace / set", () => {
    expect(buildPoolChange(inp({ mode: "add", url: "new.example.com:4444", user: "w" })).change.pool_op).toEqual({ op: "add", pool: { url: "new.example.com:4444", user: "w" } });
    expect(buildPoolChange(inp({ mode: "replace", position: "2", url: "new.example.com:4444", user: "w", password: "p" })).change.pool_op).toEqual({ op: "replace", index: 1, pool: { url: "new.example.com:4444", user: "w", password: "p" } });
    const set = buildPoolChange(inp({ mode: "set", url: "a.example.com:1", user: "w", url2: "b.example.com:2", user2: "w" })).change.pool_op;
    expect(set?.op === "set" && set.groups[0].pools.length).toBe(2);
  });
  it("rejects bad input and mixing pools with timezone", () => {
    expect(buildPoolChange(inp({ mode: "add", url: "x.example.com", user: "" })).errors).toHaveLength(2);
    expect(buildPoolChange(inp({ mode: "replace", position: "0", url: "x.example.com:1", user: "w" })).errors[0]).toMatch(/Position/);
    expect(buildPoolChange(inp({ timezone: "Not A Zone" })).errors[0]).toMatch(/IANA/);
    expect(buildPoolChange(inp({ mode: "add", url: "x.example.com:1", user: "w", timezone: "UTC" })).errors[0]).toMatch(/on its own/);
    expect(buildPoolChange(inp({})).errors[0]).toMatch(/Choose/);
  });
});

describe("planPools", () => {
  const add = inp({ mode: "add", url: "new.example.com:4444", user: "w" });
  it("lists not-supported miners with the paths tried", () => {
    const p = planPools(add, [row("192.0.2.1", { pools: sup(null), class: "antminer_stock" })], false);
    expect(p.rows[0]).toMatchObject({ included: false });
    expect(p.rows[0].note).toMatch(/not supported; tried: .*s9-cgi/);
  });
  it("respects the per-miner pool cap and positions", () => {
    const full = row("192.0.2.2", { current_pools: [{ name: "d", quota: 1, pools: [POOL, POOL, POOL] }] });
    expect(planPools(add, [full], false).rows[0].note).toMatch(/at most 3/);
    expect(planPools(inp({ mode: "replace", position: "3", url: "n.example.com:1", user: "w" }), [row("192.0.2.3")], false).rows[0].note).toMatch(/no pool at position 3/);
  });
  it("AxeOS needs exactly primary + fallback", () => {
    const axe = row("192.0.2.4", { class: "axe_os", pools_max: 2, current_pools: [{ name: "d", quota: 1, pools: [POOL, POOL] }], pools: sup("axeos-http") });
    expect(planPools(add, [axe], false).rows[0].included).toBe(false);
    expect(planPools(inp({ mode: "replace", position: "1", url: "n.example.com:1", user: "w" }), [axe], false).rows[0].included).toBe(true);
  });
  it("unconfirmed paths are dry run only", () => {
    expect(planPools(add, [row("192.0.2.5")], false).rows[0]).toMatchObject({ included: true });
    expect(planPools(add, [row("192.0.2.5")], true).rows[0]).toMatchObject({ included: false });
    expect(planPools(add, [row("192.0.2.5", { pools: sup("asic-rs", true) })], true).rows[0].included).toBe(true);
  });
  it("timezone needs a path and the class allowance", () => {
    expect(planPools(inp({ timezone: "America/Chicago" }), [row("192.0.2.6", { timezone_allowed: false })], false).rows[0].note).toMatch(/not allowed/);
    expect(planPools(inp({ timezone: "America/Chicago" }), [row("192.0.2.6", { timezone: sup(null) })], false).rows[0].note).toMatch(/timezone not supported/);
  });
});

describe("buildPoolRequest", () => {
  const confirmed = row("192.0.2.7", { pools: sup("asic-rs", true) });
  const add = inp({ mode: "add", url: "new.example.com:4444", user: "w" });
  it("dry run needs no confirmation and sets no pool OK", () => {
    const r = buildPoolRequest(planPools(add, [confirmed], false), { dryRun: true, typed: "", canarySize: 1 });
    expect(r?.dry_run).toBe(true);
    expect(r?.confirm).toBeUndefined();
  });
  it("a live pool change needs the typed text, then sets allow_pool_change", () => {
    const plan = planPools(add, [confirmed], true);
    expect(buildPoolRequest(plan, { dryRun: false, typed: "yes", canarySize: 1 })).toBeNull();
    expect(buildPoolRequest(plan, { dryRun: false, typed: POOL_CONFIRM_TEXT, canarySize: 1 })?.confirm).toEqual({ allow_pool_change: true });
  });
});

describe("describe", () => {
  it("pools and timezone in the alert text, never passwords", () => {
    expect(describePools([{ name: "d", quota: 1, pools: [POOL] }])).toBe("pool.example.com:3333 acct.worker1");
    const before = { pools: [{ name: "d", quota: 1, pools: [POOL] }] };
    const attempted = { pools: [{ name: "d", quota: 1, pools: [POOL, { url: "stratum+tcp://n.example.com:1", user: "w", password: "<redacted>" }] }] };
    const s = describeChange({ before, attempted, after: before });
    expect(s).toContain("n.example.com:1 w");
    expect(s).not.toContain("redacted");
    expect(describeChange({ before: { timezone: "UTC" }, attempted: { timezone: "America/Chicago" }, after: { timezone: "UTC" } })).toBe("timezone UTC → America/Chicago → UTC");
  });
});
