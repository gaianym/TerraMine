import { describe, expect, it } from "vitest";
import { formatHashRateDelta, formatMetricDelta, TICKER_UNITS } from "./deltaFormat";

// Demo bug 5: every up/down ticker was a bare number in hashes. Each ticker
// must carry its own metric's unit.
describe("ticker units per metric", () => {
  it("temperature in °C", () => {
    expect(formatMetricDelta("average_temperature", 3.4)).toBe("3 °C");
    expect(formatMetricDelta("average_temperature", 0.4)).toBe("0.4 °C");
  });
  it("power in W", () => {
    expect(formatMetricDelta("wattage", 12.345)).toBe("12.3 W");
  });
  it("efficiency in J/TH", () => {
    expect(formatMetricDelta("efficiency", 0.456)).toBe("0.46 J/TH");
  });
  it("fan in rpm and %", () => {
    expect(formatMetricDelta("fans", 240.4)).toBe("240 rpm");
    expect(formatMetricDelta("fan_percent", 12.6)).toBe("13%");
  });
  it("pool shares in shares", () => {
    expect(formatMetricDelta("pools_accepted", 1)).toBe("1 share");
    expect(formatMetricDelta("pools_rejected", 1200)).toBe("1,200 shares");
  });
  it("no non-hashrate ticker is denominated in hashes", () => {
    for (const col of Object.keys(TICKER_UNITS).filter((c) => c !== "hashrate")) {
      const s = formatMetricDelta(col as Exclude<keyof typeof TICKER_UNITS, "hashrate">, 5);
      expect(s).not.toMatch(/H\/s|hash/i);
      expect(s).toMatch(/[^\d.\s,]/); // has a unit
    }
  });
});

describe("hashrate ticker auto-scaled to the value column", () => {
  it("matches the value column's tier", () => {
    expect(formatHashRateDelta(5e10, "TH/s")).toBe("0.05 TH/s");
    expect(formatHashRateDelta(1.2e11, "GH/s")).toBe("120.00 GH/s");
    expect(formatHashRateDelta(3e15, "PH/s")).toBe("3.00 PH/s");
  });
  it("auto-scales when the value has no tier (0 H/s after reboot) instead of raw H", () => {
    expect(formatHashRateDelta(1.1e12, "H/s")).toBe("1.10 TH/s");
    expect(formatHashRateDelta(1.1e12, null)).toBe("1.10 TH/s");
    expect(formatHashRateDelta(4.5e11)).toBe("450.00 GH/s");
  });
  it("auto-scales tiny deltas that would print 0.00 in the value tier", () => {
    expect(formatHashRateDelta(2e9, "TH/s")).toBe("2.00 GH/s");
  });
  it("small values stay in H/s", () => {
    expect(formatHashRateDelta(12, null)).toBe("12.00 H/s");
  });
  it("always ends in a hashrate unit, never a bare number", () => {
    for (const v of [1, 1e3, 1e6, 1e9, 1e12, 1e15]) {
      expect(formatHashRateDelta(v, null)).toMatch(/ (H|kH|MH|GH|TH|PH|EH|ZH)\/s$/);
    }
  });
});

describe("every grid ticker goes through the unit-aware formatters", () => {
  it("each deltaText assignment in the page uses formatDelta / formatMetricDelta", async () => {
    const { readFileSync } = await import("node:fs");
    const { resolve } = await import("node:path");
    const page = readFileSync(resolve(__dirname, "../routes/+page.svelte"), "utf8");
    const assigns = [...page.matchAll(/deltaText:\s*([^\n]+)/g)].map((m) => m[1].trim());
    expect(assigns.length).toBeGreaterThanOrEqual(3); // movement columns, fans, pools
    for (const a of assigns) expect(a).toMatch(/^(formatDelta|formatMetricDelta)\(/);
    // and the page's formatDelta routes hashrate + each metric to the tested helpers
    expect(page).toMatch(/formatHashRateDelta\(/);
    for (const col of ["average_temperature", "wattage", "efficiency", "fans"]) {
      expect(page).toContain(`col === "${col}"`);
    }
  });
});
