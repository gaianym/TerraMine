import { describe, expect, it } from "vitest";
import { EM_DASH, formatEfficiency, formatGridValue } from "./format";

describe("formatEfficiency (demo bug: 'null' shown after reboot)", () => {
  it.each([null, undefined, "null", "", "abc", Number.NaN, Number.POSITIVE_INFINITY, {}])(
    "renders %p as an em dash",
    (v) => {
      expect(formatEfficiency(v)).toBe(EM_DASH);
      expect(formatEfficiency(v)).not.toMatch(/null/i);
    },
  );
  it("formats numbers", () => {
    expect(formatEfficiency(17.456)).toBe("17.46 J/TH");
    expect(formatEfficiency("21.5")).toBe("21.50 J/TH");
  });
});

describe("formatGridValue", () => {
  it("never renders the literal 'null'", () => {
    expect(formatGridValue(null)).toBe(EM_DASH);
    expect(formatGridValue("null")).toBe(EM_DASH);
    expect(formatGridValue(undefined)).toBe(EM_DASH);
    expect(formatGridValue("bitaxe-01")).toBe("bitaxe-01");
    expect(formatGridValue(3)).toBe("3");
  });
});
