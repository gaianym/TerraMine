import { render } from "svelte/server";
import { describe, expect, it, vi } from "vitest";
import { describeChange } from "./alertBanner";
import { EMPTY_TUNE, buildTuneChange, buildTuneRequest, restartsAxeOs } from "./tune";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => "run") }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

const tune = (o: Partial<typeof EMPTY_TUNE>) => buildTuneChange({ ...EMPTY_TUNE, ...o });

describe("buildTuneChange", () => {
  it("nothing typed = nothing to change", () => {
    expect(tune({}).errors).toEqual(["Nothing to change"]);
  });
  it("frequency + voltage", () => {
    expect(tune({ frequencyMhz: "500", coreVoltageMv: "1100" })).toEqual({ change: { frequency_mhz: 500, core_voltage_mv: 1100 }, errors: [] });
  });
  it("rejects non-integers and frequency with power", () => {
    expect(tune({ frequencyMhz: "5.5" }).errors).toEqual(["Frequency must be a whole number"]);
    expect(tune({ frequencyMhz: "500", powerW: "15" }).errors).toEqual(["Set either a frequency or a power target, not both"]);
  });
  it("manual fan needs a speed ≤ 100", () => {
    expect(tune({ fanMode: "manual", fanPct: "80" }).change).toEqual({ auto_fan: false, fan_pct: 80 });
    expect(tune({ fanMode: "manual" }).errors).toEqual(["Enter a fan speed for manual fan"]);
    expect(tune({ fanMode: "manual", fanPct: "120" }).errors).toEqual(["Fan speed cannot exceed 100"]);
  });
  it("auto fan with optional target temperature", () => {
    expect(tune({ fanMode: "auto", tempTargetC: "58" }).change).toEqual({ auto_fan: true, temp_target_c: 58 });
    expect(tune({ fanMode: "auto" }).change).toEqual({ auto_fan: true });
  });
});

describe("request", () => {
  it("dry run, raise flag and canary pass through", () => {
    const r = buildTuneRequest(["192.0.2.1", "192.0.2.2"], { frequency_mhz: 500 }, { dryRun: true, allowRaise: false, canarySize: 5 });
    expect(r).toMatchObject({ dry_run: true, confirm: { allow_frequency_raise: false }, canary_size: 2, change: { frequency_mhz: 500 } });
  });
  it("restart note for frequency / voltage / power only", () => {
    expect(restartsAxeOs({ frequency_mhz: 500 })).toBe(true);
    expect(restartsAxeOs({ power_limit_w: 15 })).toBe(true);
    expect(restartsAxeOs({ fan_pct: 80, auto_fan: false })).toBe(false);
  });
  it("alert banner shows target temperature changes", () => {
    expect(describeChange({ before: { temp_target_c: 60 }, attempted: { temp_target_c: 58 }, after: { temp_target_c: 60 } })).toBe(
      "60 → 58 → 60 °C target",
    );
  });
});

describe("TuneDialog", async () => {
  const { default: TuneDialog } = await import("./TuneDialog.svelte");
  const { default: TuneButton } = await import("./TuneButton.svelte");
  it("opens with Dry run checked and frequency raise not allowed", () => {
    const { body } = render(TuneDialog, { props: { ips: ["192.0.2.1", "192.0.2.2"], onclose: () => {} } });
    const box = (id: string) => body.match(new RegExp(`<input[^>]*data-testid="${id}"[^>]*>`))?.[0] ?? "";
    expect(box("tm-tune-dry-run")).toMatch(/\schecked(\s|=|>|\/)/);
    expect(box("tm-tune-allow-raise")).not.toMatch(/\schecked(\s|=|>|\/)/);
    expect(body).toMatch(/>\s*Dry run\s*<\/button>/);
    expect(body.match(/data-testid="tm-tune-row"/g)?.length).toBe(2);
  });
  it("button disabled with no selection", () => {
    const { body } = render(TuneButton, { props: { selectedIps: [] } });
    expect(body).toMatch(/<button[^>]*disabled/);
  });
});
