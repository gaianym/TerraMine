import { describe, expect, it } from "vitest";
import { statusChipSpec } from "./statusChip";

describe("statusChipSpec", () => {
  it("shows a distinct 'dry run' badge", () => {
    const c = statusChipSpec({ action: "reboot", status: "dryrun", msg: null });
    expect(c.text).toBe("dry run");
    expect(c.tone).toBe("dry");
    expect(c.className).toBe("tm-chip tm-chip-dry");
  });
  it("says waiting for hashrate until the re-poll confirms the reboot", () => {
    const row = { action: "reboot" as const, status: "verified" as const, msg: null };
    expect(statusChipSpec(row, "polling").text).toBe("waiting for hashrate…");
    expect(statusChipSpec(row, "online").text).toBe("back online");
    expect(statusChipSpec(row, "timeout").tone).toBe("bad");
  });
  it("always carries the base tm-chip class (legibility CSS hangs off it)", () => {
    for (const status of ["queued", "sent", "verifying", "verified", "failed", "dryrun", "unsupported"] as const) {
      expect(statusChipSpec({ action: "stop", status, msg: null }).className).toMatch(/^tm-chip tm-chip-[a-z]+$/);
    }
  });
});
