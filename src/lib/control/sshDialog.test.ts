import { render } from "svelte/server";
import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => []) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

const { default: SshDialog } = await import("./SshDialog.svelte");

const support = [
  {
    ip: "192.0.2.10", found: true, class: "antminer_stock", model: "Antminer S9",
    actions: (["restart", "read_logs", "grant_api_write"] as const).map((action) => ({ action, supported: true, reason: null, hw_confirmed: false })),
    access: { username: null, auth: null, pinned_fingerprint: null },
    native_stop: false, native_start: false, diverted: false,
  },
];

describe("SshDialog", () => {
  it("opens with Dry run checked, the plan per miner and a masked password field", () => {
    const { body } = render(SshDialog, { props: { ips: ["192.0.2.10"], onclose: () => {}, initialSupport: support } });
    const dry = body.match(/<input[^>]*data-testid="tm-dry-run"[^>]*>/)?.[0] ?? "";
    expect(dry).toMatch(/\schecked(\s|=|>|\/)/);
    expect(body).toMatch(/>\s*Dry run\s*<\/button>/);
    expect(body).toContain("dry run only until confirmed on hardware");
    expect(body).toContain("host key not pinned yet");
    expect(body).toMatch(/type="password"[^>]*data-testid="tm-ssh-password"|data-testid="tm-ssh-password"[^>]*type="password"/);
    expect(body).toContain("Check &amp; trust");
  });
});
