import { describe, expect, it } from "vitest";
import { GROUP_TEXT, columns, firmwareKey, groupByFirmware, summarize, type MatrixCell, type MatrixRow } from "./matrix";

const cell = (action: string, status: MatrixCell["status"], path: string | null = null): MatrixCell => ({
  action, group: action === "light_on" ? "control" : "settings", label: action, status, path, paths_tried: [`tried ${action}`],
});
const row = (ip: string, firmware: string, cells: MatrixCell[], found = true): MatrixRow => ({
  ip, found, class: "axeos", make: "Bitaxe", model: "Gamma", firmware, driver: "axeos", cells, native: [], error: found ? null : "offline",
});

describe("capability matrix helpers", () => {
  const rows = [
    row("192.0.2.1", "2.15.0", [cell("light_on", "dry_run_only", "axeos"), cell("frequency", "confirmed", "axeos")]),
    row("192.0.2.2", "2.15.0", [cell("light_on", "dry_run_only", "axeos"), cell("frequency", "dry_run_only", "asic-rs")]),
    row("192.0.2.3", "2.10.1", [cell("light_on", "unsupported"), cell("frequency", "unsupported")]),
    row("192.0.2.9", "?", [], false),
  ];

  it("takes columns from the backend order", () => {
    expect(columns(rows).map((c) => c.action)).toEqual(["light_on", "frequency"]);
    expect(columns([])).toEqual([]);
  });

  it("groups by class, model and firmware, marking mixed cells", () => {
    const g = groupByFirmware(rows);
    expect(g.map((x) => x.firmware)).toEqual(["2.10.1", "2.15.0"]);
    const g215 = g[1];
    expect(g215.ips).toEqual(["192.0.2.1", "192.0.2.2"]);
    expect(g215.cells.light_on.status).toBe("dry_run_only");
    expect(g215.cells.frequency.status).toBe("mixed");
    expect(g215.cells.frequency.counts).toEqual({ confirmed: 1, dry_run_only: 1, unsupported: 0 });
    expect(g215.cells.frequency.paths).toEqual(["axeos", "asic-rs"]);
    expect(g215.cells.frequency.members).toHaveLength(2);
    expect(firmwareKey(rows[0])).toBe("axeos | Gamma | 2.15.0");
  });

  it("summarizes totals and missing miners", () => {
    expect(summarize(rows)).toEqual({ confirmed: 1, dry_run_only: 3, unsupported: 2, miners: 3, missing: 1 });
  });

  it("keeps admin and fallback cells in their own groups", () => {
    const r = row("192.0.2.4", "2018", [
      { ...cell("factory_reset", "dry_run_only", "s9-cgi"), group: "admin" },
      { ...cell("ssh_grant_api_write", "dry_run_only", "ssh (Antminer stock (bmminer))"), group: "fallback" },
      { ...cell("pool_divert", "unsupported"), group: "fallback" },
    ]);
    expect(columns([r]).map((c) => c.group)).toEqual(["admin", "fallback", "fallback"]);
    expect(summarize([r])).toEqual({ confirmed: 0, dry_run_only: 2, unsupported: 1, miners: 1, missing: 0 });
    expect(Object.keys(GROUP_TEXT)).toEqual(["control", "settings", "admin", "fallback"]);
  });
});
