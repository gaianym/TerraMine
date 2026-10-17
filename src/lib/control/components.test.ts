import { render } from "svelte/server";
import { describe, expect, it, vi } from "vitest";

// No Tauri runtime in tests; effects don't run during SSR, but keep imports inert.
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => []) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

const { default: ActionDialog } = await import("./ActionDialog.svelte");
const { default: ControlToolbar } = await import("./ControlToolbar.svelte");
const { control } = await import("./controlState.svelte");

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
