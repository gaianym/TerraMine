import { describe, expect, it } from "vitest";
import { ALL_ACTIONS, affectsHashing, dryRunOnly, unsupportedReason, type CapsRow } from "./types";

const row = (caps: Partial<CapsRow["caps"]>, why?: CapsRow["why"]): CapsRow => ({
  ip: "192.0.2.60",
  found: true,
  caps: { start: false, stop: false, reboot: false, ...caps },
  why,
});

describe("action capability helpers", () => {
  it("covers the fault light in the action list", () => {
    expect(ALL_ACTIONS).toEqual(["start", "stop", "reboot", "light_on", "light_off"]);
  });

  it("re-polls hashrate only for actions that change hashing", () => {
    expect(ALL_ACTIONS.filter(affectsHashing)).toEqual(["start", "stop", "reboot"]);
  });

  it("explains an unsupported light with the backend's paths tried", () => {
    const r = row({ light_on: false }, { light_on: "axeos-http: no identify endpoint before AxeOS 2.12.0" });
    expect(unsupportedReason(r, "light_on")).toBe("No fault-light path; tried: axeos-http: no identify endpoint before AxeOS 2.12.0");
    expect(unsupportedReason(row({ light_off: false }), "light_off")).toBe("No fault-light path for this firmware");
    expect(unsupportedReason(row({ light_on: true }), "light_on")).toBeNull();
    expect(unsupportedReason(undefined, "light_on")).toBeNull();
  });

  it("keeps the pre-existing reboot / pause texts when no paths are given", () => {
    expect(unsupportedReason(row({}), "reboot")).toBe("Firmware exposes no reboot API");
    expect(unsupportedReason(row({}, { stop: "s9-cgi: no miner-mode" }), "stop")).toBe("Not supported; tried: s9-cgi: no miner-mode");
  });

  it("flags unconfirmed routes as dry run only", () => {
    expect(dryRunOnly(row({ light_on: true, unconfirmed: ["light_on"] }), "light_on")).toBe(true);
    expect(dryRunOnly(row({ light_on: true }), "light_on")).toBe(false);
    expect(dryRunOnly(row({ light_on: false, unconfirmed: ["light_on"] }), "light_on")).toBe(false);
  });
});
