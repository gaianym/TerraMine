import { render } from "svelte/server";
import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => []) }));

const { default: CapabilityMatrix } = await import("./CapabilityMatrix.svelte");
const { default: AuditLogViewer } = await import("./AuditLogViewer.svelte");
const { default: HardeningButtons } = await import("./HardeningButtons.svelte");

const rows = [
  {
    ip: "192.0.2.10", found: true, class: "axeos", make: "Bitaxe", model: "Gamma", firmware: "2.15.0", driver: "axeos",
    cells: [
      { action: "frequency", group: "settings", label: "Frequency", status: "confirmed", path: "axeos", paths_tried: ["axeos: confirmed"] },
      { action: "light_on", group: "control", label: "Light on", status: "unsupported", path: null, paths_tried: ["supports_set_fault_light = false"] },
    ],
    native: [{ name: "supports_set_fault_light", supported: false }],
    error: null,
  },
  { ip: "192.0.2.11", found: false, class: null, make: null, model: null, firmware: null, driver: null, cells: [], native: [], error: "not in cache" },
] as const;

describe("CapabilityMatrix", () => {
  it("renders per-miner rows, statuses and totals from backend data", () => {
    const { body } = render(CapabilityMatrix, { props: { ips: [], onclose: () => {}, initialRows: rows as never } });
    expect(body).toContain("all discovered miners");
    expect(body.match(/data-testid="tm-matrix-row"/g)).toHaveLength(2);
    expect(body).toContain("✓ live · axeos");
    expect(body).toContain("not found: not in cache");
    expect(body).toContain("1 live");
    expect(body).toContain("asic-rs flags");
  });
});

describe("AuditLogViewer", () => {
  it("renders records with filters and export, never secrets", () => {
    const page = {
      records: [{
        ts_ms: 1_792_500_000_123, run_id: "r1", ip: "192.0.2.10", action: "set_pools", phase: "outcome", dry_run: true,
        irreversible: false, device_class: "axeos", model: "Gamma", firmware: "2.15.0",
        requested: { password: "<redacted>" }, snapshot_id: null, pre_state: null, paths_tried: [], outcome: "dryrun", msg: "would send",
      }],
      total: 1, skipped_lines: 1, ips: ["192.0.2.10"], actions: ["set_pools"], outcomes: ["dryrun"],
    };
    const { body } = render(AuditLogViewer, { props: { onclose: () => {}, initialPage: page } });
    expect(body).toContain("1 of 1");
    expect(body).toContain("1 unreadable line(s) skipped");
    expect(body).toContain("Export CSV");
    expect(body).toContain('data-testid="tm-audit-row"');
    expect(body).toContain("set_pools");
    expect(body).not.toContain("hunter2");
  });
});

describe("HardeningButtons", () => {
  it("renders both toolbar buttons", () => {
    const { body } = render(HardeningButtons, { props: { selectedIps: [] } });
    expect(body).toContain("Capabilities…");
    expect(body).toContain("Audit log…");
  });
});
