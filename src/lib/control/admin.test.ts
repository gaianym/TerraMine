import { describe, expect, it } from "vitest";
import {
  ADMIN_ACTIONS,
  type AdminSupportRow,
  boardDegraded,
  buildAdminRequest,
  confirmText,
  describeBoard,
  isRead,
  passwordError,
  planAdmin,
} from "./admin";

const sup = (ip: string, path: string | null, confirmed = false): AdminSupportRow => ({
  ip,
  found: true,
  class: "AntminerStock",
  model: "S19",
  firmware: "Stock",
  device_login: false,
  actions: Object.fromEntries(
    ADMIN_ACTIONS.map((a) => [a, { path, confirmed: isRead(a) || confirmed, tried: path ? [`${path}: selected`] : ["axeos: skipped (AxeOS has no login)"] }]),
  ),
});

const ok = { dryRun: true, typed: "", acknowledged: false, password: "", passwordAgain: "" };

describe("admin plan", () => {
  it("reads run fleetwide with no dry run or confirmation", () => {
    const p = planAdmin("read_logs", [sup("192.0.2.1", "asic-rs"), sup("192.0.2.2", "axeos")], false);
    expect(p.rows.every((r) => r.included)).toBe(true);
    const req = buildAdminRequest("read_logs", p, ok)!;
    expect(req.dry_run).toBe(false);
    expect(req.confirm).toBeUndefined();
    expect(req.ips).toHaveLength(2);
  });

  it("unsupported rows are excluded with the paths tried", () => {
    const p = planAdmin("factory_reset", [sup("192.0.2.1", null), { ...sup("192.0.2.3", null), found: false }], false);
    expect(p.rows[0]).toMatchObject({ included: false });
    expect(p.rows[0].note).toContain("AxeOS has no login");
    expect(p.rows[1].note).toContain("not reachable");
    expect(buildAdminRequest("factory_reset", p, ok)).toBeNull();
  });

  it("destructive actions dry-run fleetwide; live needs one confirmed miner, typed name and the checkbox", () => {
    const fleet = [sup("192.0.2.1", "asic-rs"), sup("192.0.2.2", "asic-rs")];
    const dry = buildAdminRequest("factory_reset", planAdmin("factory_reset", fleet, false), ok)!;
    expect(dry.dry_run).toBe(true);
    expect(dry.ips).toHaveLength(2);
    // Unconfirmed paths: dry run only.
    const live = planAdmin("factory_reset", fleet, true);
    expect(live.rows.every((r) => !r.included)).toBe(true);
    expect(live.rows[0].note).toContain("not yet confirmed on real hardware");
    expect(live.errors[0]).toContain("No miner");
    // A (future) confirmed path: one device only.
    expect(planAdmin("factory_reset", [sup("192.0.2.1", "x", true), sup("192.0.2.2", "x", true)], true).errors[0]).toContain("exactly one");
    const one = planAdmin("factory_reset", [sup("192.0.2.1", "x", true)], true);
    const go = { ...ok, dryRun: false, acknowledged: true, typed: "factory_reset" };
    expect(buildAdminRequest("factory_reset", one, { ...go, typed: "factory reset" })).toBeNull();
    expect(buildAdminRequest("factory_reset", one, { ...go, acknowledged: false })).toBeNull();
    expect(buildAdminRequest("factory_reset", one, go)?.confirm).toEqual({ typed: "factory_reset", allow_irreversible: true });
  });

  it("password: typed twice, printable, sent only live", () => {
    expect(passwordError("", "")).toMatch(/Enter/);
    expect(passwordError("abc", "abd")).toMatch(/match/);
    expect(passwordError("a".repeat(65), "a".repeat(65))).toMatch(/64/);
    expect(passwordError("a\tb", "a\tb")).toMatch(/control/);
    expect(passwordError("good-one", "good-one")).toBeNull();
    const p = planAdmin("change_password", [sup("192.0.2.1", "asic-rs")], false);
    expect(buildAdminRequest("change_password", p, { ...ok, password: "x", passwordAgain: "x" })?.new_password).toBeUndefined();
    const live = planAdmin("change_password", [sup("192.0.2.1", "x", true)], true);
    const go = { dryRun: false, acknowledged: true, typed: confirmText("change_password"), password: "pw1", passwordAgain: "pw2" };
    expect(buildAdminRequest("change_password", live, go)).toBeNull();
    expect(buildAdminRequest("change_password", live, { ...go, passwordAgain: "pw1" })?.new_password).toBe("pw1");
  });

  it("describes boards", () => {
    expect(describeBoard({ position: 0, working_chips: 29, expected_chips: 63, hashrate_ghs: 2122.7, outlet_chip_temp_c: 75 })).toBe(
      "29/63 chips · 2.12 TH/s · 75°C",
    );
    expect(boardDegraded({ position: 0, working_chips: 29, expected_chips: 63 })).toBe(true);
    expect(boardDegraded({ position: 0, working_chips: 63, expected_chips: 63, hashrate_ghs: 4600 })).toBe(false);
    expect(describeBoard({ position: 1 })).toBe("no data");
  });
});
