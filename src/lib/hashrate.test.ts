import { describe, expect, it } from "vitest";
import {
  formatHashRateValue,
  HASH_RATE_UNIT_TIERS,
  normalizeHashRateUnit,
  parseHashRateToHs,
} from "./hashrate";
import { formatHashRateDelta } from "./deltaFormat";

// Demo bug: a Bitaxe row showed "979.20 GHz/s". Every displayed hashrate unit
// must be one of the canonical strings below; "Hz" must never appear.
const CANONICAL = ["H/s", "kH/s", "MH/s", "GH/s", "TH/s", "PH/s", "EH/s", "ZH/s"];
const UNIT_RE = /^-?\d+\.\d{2} (H|kH|MH|GH|TH|PH|EH|ZH)\/s$/;

describe("hashrate unit strings", () => {
  it("uses exactly the canonical tiers", () => {
    expect([...HASH_RATE_UNIT_TIERS]).toEqual(CANONICAL);
  });

  it.each([
    ["GHz", "GH/s"], ["GHz/s", "GH/s"], ["ghz", "GH/s"], ["GH/s", "GH/s"], ["GH", "GH/s"], ["ghs", "GH/s"],
    ["GigaHash", "GH/s"], ["giga hash", "GH/s"], ["Gh/sec", "GH/s"],
    ["H/s", "H/s"], ["Hash", "H/s"], ["hs", "H/s"],
    ["KH/s", "kH/s"], ["kH/s", "kH/s"], ["KiloHash", "kH/s"], ["khz", "kH/s"],
    ["MH/s", "MH/s"], ["MegaHash", "MH/s"], ["MHz/s", "MH/s"],
    ["TH/s", "TH/s"], ["TeraHash", "TH/s"], ["THz", "TH/s"], ["ths", "TH/s"],
    ["PH/s", "PH/s"], ["PetaHash", "PH/s"],
    ["EH/s", "EH/s"], ["ZettaHash", "ZH/s"],
  ])("normalises %s -> %s", (raw, want) => {
    expect(normalizeHashRateUnit(raw)).toBe(want);
  });

  it("rejects non-hashrate units", () => {
    for (const raw of ["W", "J/TH", "rpm", "°C", "", "GB"]) expect(normalizeHashRateUnit(raw)).toBeNull();
  });

  it("displays 979.2 GHz/s (string or object) as 979.20 GH/s", () => {
    expect(formatHashRateValue("979.2 GHz/s")).toBe("979.20 GH/s");
    expect(formatHashRateValue("979.2 GHz")).toBe("979.20 GH/s");
    expect(formatHashRateValue({ value: 979.2, unit: "GHz" })).toBe("979.20 GH/s");
    expect(formatHashRateValue({ value: 979.2, unit: "GigaHash", algo: "SHA256" })).toBe("979.20 GH/s");
    expect(parseHashRateToHs({ value: 979.2, unit: "GHz" })).toBeCloseTo(979.2e9);
  });

  it("auto-scales into each tier with a canonical unit", () => {
    const cases: Array<[number, string]> = [
      [12, "12.00 H/s"], [1.5e3, "1.50 kH/s"], [2.5e6, "2.50 MH/s"], [979.2e9, "979.20 GH/s"],
      [1.1e12, "1.10 TH/s"], [3e15, "3.00 PH/s"], [4e18, "4.00 EH/s"], [5e21, "5.00 ZH/s"],
    ];
    for (const [hs, want] of cases) {
      expect(formatHashRateValue(hs)).toBe(want);
      expect(formatHashRateValue(hs)).toMatch(UNIT_RE);
    }
    expect(formatHashRateValue({ value: 1100, unit: "GigaHash" })).toBe("1.10 TH/s");
  });

  it("never prints Hz in values or tickers", () => {
    for (const unit of ["GHz", "GHz/s", "THz", "MHz/s", "khz"]) {
      expect(formatHashRateValue({ value: 979.2, unit })).not.toMatch(/Hz/);
      expect(formatHashRateDelta(5e10, unit)).not.toMatch(/Hz/);
    }
  });

  it("unknown units are not silently treated as H/s", () => {
    expect(parseHashRateToHs({ value: 5, unit: "W" })).toBeNull();
    expect(formatHashRateValue({ value: 5, unit: "W" })).toBe("\u2014");
  });
});
