import { describe, expect, it } from "vitest";
import { GRANT_CONFIRM_TEXT, buildDivertJob, buildSshJob, cidrError, planFallback, statusTone, trustPrompt, type SshOutcome, type SshSupportRow } from "./ssh";

function row(ip: string, over: Partial<SshSupportRow> = {}): SshSupportRow {
  return {
    ip, found: true, class: "antminer_stock", model: "Antminer S9",
    actions: [
      { action: "restart", supported: true, reason: null, hw_confirmed: false },
      { action: "read_logs", supported: true, reason: null, hw_confirmed: false },
      { action: "grant_api_write", supported: true, reason: null, hw_confirmed: false },
    ],
    access: { username: "root", auth: "password", pinned_fingerprint: null },
    native_stop: false, native_start: false, diverted: false, ...over,
  };
}
const lux = row("192.0.2.2", {
  class: "lux_os",
  actions: (["restart", "read_logs", "grant_api_write"] as const).map((action) => ({ action, supported: false, reason: "LuxOS documents no shell commands", hw_confirmed: false })),
});

describe("cidr", () => {
  it("accepts a LAN range, rejects junk and 0/0", () => {
    expect(cidrError("192.0.2.0/24")).toBeNull();
    expect(cidrError("192.0.2.0")).toMatch(/a\.b\.c\.0\/24/);
    expect(cidrError("300.0.2.0/24")).toMatch(/out of bounds/);
    expect(cidrError("0.0.0.0/0")).toMatch(/whole internet/);
  });
});

describe("planFallback", () => {
  it("per-firmware gate with reasons, dry-run-only note and host-key state", () => {
    const p = planFallback("restart", [row("192.0.2.1"), lux], false);
    expect(p.rows[0]).toMatchObject({ included: true });
    expect(p.rows[0].note).toMatch(/dry run only until confirmed/);
    expect(p.rows[0].note).toMatch(/host key not pinned/);
    expect(p.rows[1]).toEqual({ ip: "192.0.2.2", included: false, note: "LuxOS documents no shell commands" });
  });
  it("grant needs a range and one miner when live", () => {
    expect(planFallback("grant_api_write", [row("192.0.2.1")], false, "").errors[0]).toMatch(/LAN range/);
    expect(planFallback("grant_api_write", [row("192.0.2.1"), row("192.0.2.3")], true, "192.0.2.0/24").errors[0]).toMatch(/exactly one/);
    expect(planFallback("grant_api_write", [row("192.0.2.1")], true, "192.0.2.0/24").errors).toEqual([]);
  });
  it("divert only where there is no native stop; restore only after a divert", () => {
    const rows = [row("192.0.2.1"), row("192.0.2.4", { native_stop: true }), row("192.0.2.5", { diverted: true })];
    expect(planFallback("divert", rows, false).rows.map((r) => r.included)).toEqual([true, false, false]);
    expect(planFallback("restore", rows, false).rows.map((r) => r.included)).toEqual([false, false, true]);
  });
});

describe("requests", () => {
  it("ssh job: typed + allow only for a live grant", () => {
    expect(buildSshJob("restart", ["192.0.2.1"], { dryRun: true, typed: "x", cidr: "" })).toMatchObject({ dry_run: true, typed: null, allow_irreversible: false, cidr: null });
    expect(buildSshJob("grant_api_write", ["192.0.2.1"], { dryRun: false, typed: ` ${GRANT_CONFIRM_TEXT} `, cidr: "192.0.2.0/24" })).toMatchObject({
      dry_run: false, typed: GRANT_CONFIRM_TEXT, allow_irreversible: true, cidr: "192.0.2.0/24",
    });
  });
  it("divert job carries the pool OK only when live", () => {
    expect(buildDivertJob("divert", ["192.0.2.1"], true).confirm.allow_pool_change).toBe(false);
    expect(buildDivertJob("restore", ["192.0.2.1"], false).confirm.allow_pool_change).toBe(true);
  });
  it("trust prompt only for first-use outcomes", () => {
    const o: SshOutcome = { ip: "192.0.2.1", action: "restart", status: "host_key_unpinned", msg: "", plan: [], paths_tried: [], fingerprint: "SHA256:abc", output: null, backup: null };
    expect(trustPrompt(o)).toEqual({ ip: "192.0.2.1", fingerprint: "SHA256:abc" });
    expect(trustPrompt({ ...o, status: "host_key_changed" })).toBeNull();
    expect(statusTone("host_key_changed")).toBe("bad");
    expect(statusTone("dry_run")).toBe("info");
  });
});
