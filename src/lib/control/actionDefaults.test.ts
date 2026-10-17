import { describe, expect, it } from "vitest";
import { DEFAULT_ACTION_OPTIONS, defaultActionOptions } from "./actionDefaults";

describe("action dialog defaults (demo bug: dry run unchecked)", () => {
  it("defaults dry run to ON", () => {
    expect(DEFAULT_ACTION_OPTIONS.dryRun).toBe(true);
    expect(defaultActionOptions().dryRun).toBe(true);
  });
  it("returns a fresh copy each time so one dialog cannot flip another's default", () => {
    const a = defaultActionOptions();
    a.dryRun = false;
    expect(defaultActionOptions().dryRun).toBe(true);
    expect(Object.isFrozen(DEFAULT_ACTION_OPTIONS)).toBe(true);
  });
});
