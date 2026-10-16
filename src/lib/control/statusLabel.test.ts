import { describe, expect, it } from "vitest";
import {
  formatActionStatusLabel,
  ipsNeedingRefresh,
  missingAgGridModules,
  REQUIRED_AG_GRID_MODULE_IDS,
} from "./statusLabel";

describe("formatActionStatusLabel", () => {
  it("shows sent / verifying / back online for reboot", () => {
    expect(formatActionStatusLabel({ action: "reboot", status: "sent" })).toBe("sent");
    expect(formatActionStatusLabel({ action: "reboot", status: "verifying" })).toBe("rebooting…");
    expect(formatActionStatusLabel({ action: "reboot", status: "rebooting" })).toBe("rebooting…");
    expect(formatActionStatusLabel({ action: "reboot", status: "verified" })).toBe("back online");
  });

  it("shows verified paused / verified running", () => {
    expect(formatActionStatusLabel({ action: "stop", status: "verified" })).toBe("verified paused");
    expect(formatActionStatusLabel({ action: "start", status: "verified" })).toBe("verified running");
  });

  it("shows failed", () => {
    expect(formatActionStatusLabel({ action: "reboot", status: "failed" })).toBe("failed");
  });
});

describe("ipsNeedingRefresh", () => {
  it("returns ips that finished an action that may change miner state", () => {
    expect(
      ipsNeedingRefresh([
        { ip: "192.0.2.1", status: "verified" },
        { ip: "192.0.2.2", status: "unsupported" },
        { ip: "192.0.2.3", status: "failed" },
        { ip: "192.0.2.1", status: "ok" },
      ]),
    ).toEqual(["192.0.2.1"]);
  });

  it("includes rebooting so a follow-up poll can catch the miner coming back", () => {
    expect(ipsNeedingRefresh([{ ip: "192.0.2.9", status: "rebooting" }])).toEqual(["192.0.2.9"]);
  });
});

describe("RowSelectionModule registration requirement", () => {
  it("lists RowSelectionModule as required (demo: checkboxes missing without it)", () => {
    expect(REQUIRED_AG_GRID_MODULE_IDS).toContain("RowSelectionModule");
    expect(missingAgGridModules(["ClientSideRowModelModule", "ColumnApiModule"])).toEqual([
      "RowSelectionModule",
    ]);
    expect(
      missingAgGridModules([
        "ClientSideRowModelModule",
        "ColumnApiModule",
        "RowSelectionModule",
      ]),
    ).toEqual([]);
  });
});
