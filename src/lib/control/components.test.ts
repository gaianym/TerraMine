import { render } from "svelte/server";
import { describe, expect, it, vi } from "vitest";

// No Tauri runtime in tests; effects don't run during SSR, but keep imports inert.
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => []) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

const { default: ActionDialog } = await import("./ActionDialog.svelte");
const { default: ControlToolbar } = await import("./ControlToolbar.svelte");
const { control } = await import("./controlState.svelte");
const { default: SafetyAlertBanner } = await import("./SafetyAlertBanner.svelte");
const { sampleAlert } = await import("./alertFixtures");

function dryRunInput(html: string): string {
  const m = html.match(/<input[^>]*data-testid="tm-dry-run"[^>]*>/);
  if (!m) throw new Error("dry-run checkbox not rendered");
  return m[0];
}

describe("ActionDialog renders with Dry run checked (per-row and bulk)", () => {
  const cases: Array<[string, string[]]> = [
    ["per-row", ["192.0.2.10"]],
    ["bulk (5)", ["192.0.2.1", "192.0.2.2", "192.0.2.3", "192.0.2.4", "192.0.2.5"]],
    ["bulk (>typed threshold)", Array.from({ length: 8 }, (_, i) => `192.0.2.${20 + i}`)],
  ];
  for (const action of ["start", "stop", "reboot"] as const) {
    for (const [name, ips] of cases) {
      it(`${action} ${name}`, () => {
        const { body } = render(ActionDialog, { props: { ips, action, onclose: () => {} } });
        expect(dryRunInput(body)).toMatch(/\schecked(\s|=|>|\/)/);
        // Confirm button reflects dry run, not the live action.
        expect(body).toMatch(/>\s*Dry run\s*<\/button>/);
      });
    }
  }
});

describe("ControlToolbar run summary", () => {
  it("shows 'Dry run: 5/5 would be sent' for a dry run, not 'Last run: 0/5 ok'", () => {
    control.activeRunId = null;
    control.lastDone = {
      run_id: "r1",
      results: Array.from({ length: 5 }, (_, i) => ({
        ip: `192.0.2.${i + 1}`,
        action: "reboot" as const,
        status: "dryrun" as const,
        msg: null,
      })),
    };
    const { body } = render(ControlToolbar, { props: { selectedIps: [], onrequest: () => {} } });
    expect(body).toContain("Dry run: 5/5 would be sent");
    expect(body).toMatch(/data-kind="dry"/);
    expect(body).not.toContain("Last run: 0/5 ok");
  });
});

describe("SafetyAlertBanner", () => {
  it("renders the grouped headline, retry and dismiss", () => {
    const { body } = render(SafetyAlertBanner, { props: { initial: [sampleAlert()] } });
    expect(body).toContain("3 of 4 rolled back");
    expect(body).toMatch(/Retry 3 miners \(dry run\)/);
    expect(body).toContain("Dismiss");
    expect(body).toMatch(/data-tone="warn"/);
  });

  it("shows the likely-external note", () => {
    const note = "likely external (network/pool): 2 of 2 failures coincided with degradation on untouched miners (192.0.2.9)";
    const { body } = render(SafetyAlertBanner, {
      props: { initial: [sampleAlert({ likely_external: true, external_note: note })] },
    });
    expect(body).toContain(note);
  });

  it("renders nothing without alerts", () => {
    const { body } = render(SafetyAlertBanner, { props: {} });
    expect(body).not.toContain("tm-safety-alert");
  });
});

describe("fault light (PR B)", () => {
  for (const action of ["light_on", "light_off"] as const) {
    it(`${action} dialog opens with Dry run checked and explains the light`, () => {
      const { body } = render(ActionDialog, { props: { ips: ["192.0.2.30", "192.0.2.31"], action, onclose: () => {} } });
      expect(dryRunInput(body)).toMatch(/\schecked(\s|=|>|\/)/);
      expect(body).toMatch(/>\s*Dry run\s*<\/button>/);
      expect(body).toContain('data-testid="tm-light-note"');
    });
  }

  it("lists unsupported miners with the paths tried and flags dry-run-only routes", () => {
    control.capsByIp = {
      "192.0.2.40": {
        ip: "192.0.2.40",
        found: true,
        caps: { start: true, stop: true, reboot: true, light_on: false, light_off: false },
        why: { light_on: "asic-rs: supports_set_fault_light() = false on WhatsMiner M20 (Whatsminer v1)" },
      },
      "192.0.2.41": {
        ip: "192.0.2.41",
        found: true,
        caps: { start: false, stop: false, reboot: true, light_on: true, light_off: true, unconfirmed: ["light_on", "light_off"] },
      },
    };
    const { body } = render(ActionDialog, {
      props: { ips: ["192.0.2.40", "192.0.2.41"], action: "light_on", onclose: () => {} },
    });
    expect(body).toContain("No fault-light path; tried: asic-rs: supports_set_fault_light() = false");
    expect(body).toContain("dry run only: route not yet confirmed on real hardware");
    expect(body).toMatch(/Light on 1 of 2 miners/);
    control.capsByIp = {};
  });

  it("toolbar offers Light on / Light off as bulk actions", () => {
    control.activeRunId = null;
    control.lastDone = null;
    const { body } = render(ControlToolbar, { props: { selectedIps: ["192.0.2.50"], onrequest: () => {} } });
    expect(body).toContain("Light on");
    expect(body).toContain("Light off");
  });
});

describe("AdminDialog (plan step G)", async () => {
  const { default: AdminDialog } = await import("./AdminDialog.svelte");
  const { default: AdminButton } = await import("./AdminButton.svelte");
  const { ADMIN_ACTIONS } = await import("./admin");
  type Row = import("./admin").AdminSupportRow;
  const row = (ip: string, path: string | null): Row => ({
    ip,
    found: true,
    class: "AntminerStock",
    model: "Antminer S19",
    firmware: "AntMiner Stock",
    device_login: false,
    actions: Object.fromEntries(ADMIN_ACTIONS.map((a) => [a, { path, confirmed: a === "hashboards" || a === "read_logs", tried: [] }])),
  });

  for (const action of ["change_password", "factory_reset"] as const) {
    it(`${action} opens with Dry run checked, no typed box, no password fields`, () => {
      const { body } = render(AdminDialog, {
        props: { ips: ["192.0.2.60", "192.0.2.61"], initialAction: action, initialSupport: [row("192.0.2.60", "asic-rs"), row("192.0.2.61", null)], onclose: () => {} },
      });
      const m = body.match(/<input[^>]*data-testid="tm-admin-dry-run"[^>]*>/);
      expect(m?.[0]).toMatch(/\schecked(\s|=|>|\/)/);
      expect(body).not.toContain('data-testid="tm-admin-typed"');
      expect(body).not.toContain('data-testid="tm-admin-password"');
      expect(body).toMatch(/data-testid="tm-admin-send"[^>]*>(\s|<!--[^>]*-->)*Dry run(\s|<!--[^>]*-->)*<\/button>/);
      expect(body).toContain("not yet confirmed on real hardware");
      expect(body).toContain("not supported");
    });
  }

  it("reads have no dry-run box and run on every supported miner", () => {
    const { body } = render(AdminDialog, {
      props: { ips: ["192.0.2.62"], initialAction: "read_logs", initialSupport: [row("192.0.2.62", "asic-rs")], onclose: () => {} },
    });
    expect(body).not.toContain('data-testid="tm-admin-dry-run"');
    expect(body).toMatch(/Read logs (<!--[^>]*-->)?\((<!--[^>]*-->)?1(<!--[^>]*-->)?\)/);
    expect(body).toContain("never written to the audit log");
  });

  it("toolbar button needs a selection", () => {
    expect(render(AdminButton, { props: { selectedIps: [] } }).body).toMatch(/data-testid="tm-admin-open"[^>]*disabled|disabled[^>]*data-testid="tm-admin-open"/);
    expect(render(AdminButton, { props: { selectedIps: ["192.0.2.63"] } }).body).toContain("Admin…");
  });
});
