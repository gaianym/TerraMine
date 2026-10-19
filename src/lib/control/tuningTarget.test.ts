import { render } from "svelte/server";
import { describe, expect, it, vi } from "vitest";
import { describeChange } from "./alertBanner";
import { buildSettingsRequest, type SettingsSupportRow } from "./power";
import { buildTargetChange, commonPresets, planTarget, thsToGhs, type TargetInput } from "./tuningTarget";
import TuningTargetDialog from "./TuningTargetDialog.svelte";
import TuningTargetButton from "./TuningTargetButton.svelte";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => []) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

const sup = (path: string | null, confirmed = false, tried: string[] = []) => ({ path, confirmed, tried });
const no = (why: string) => sup(null, false, [`asic-rs: skipped (${why})`]);
const base = { found: true, power_w: null, tuning_pct: null, power: sup(null), tuning: sup(null) };

const antminer: SettingsSupportRow = {
  ...base, ip: "192.0.2.1", class: "antminer_stock", model: "Antminer S19",
  tuning_modes: ["low", "normal"], mode: sup("asic-rs"), hashrate: no("hashrate target"), preset: no("supports_presets() = false"), scaling: no("scaling"),
};
const epic: SettingsSupportRow = {
  ...base, ip: "192.0.2.2", class: "epic", model: "Antminer S19",
  tuning_modes: [], tuning_hashrate_ghs: [50000, 110000], scaling_step: [1, 10], scaling_min: [50, 100],
  mode: no("mining mode target"), hashrate: sup("asic-rs"), preset: no("supports_presets() = false"), scaling: sup("asic-rs"),
};
const vnish: SettingsSupportRow = {
  ...base, ip: "192.0.2.3", class: "vnish", model: "Antminer S19",
  tuning_modes: [], mode: no("mining mode"), hashrate: no("hashrate"), preset: sup("asic-rs"), scaling: no("scaling"), presets: ["2500", "3000"],
};
const bitaxe: SettingsSupportRow = {
  ...base, ip: "192.0.2.4", class: "axe_os", model: "Gamma",
  tuning_modes: ["low", "normal"], tuning_hashrate_ghs: [300, 1400],
  mode: sup("axeos-http"), hashrate: sup("axeos-http"), preset: no("AxeOS has no tuning presets"), scaling: no("AxeOS has no scaling"),
};
const rows = [antminer, epic, vnish, bitaxe];

const input = (p: Partial<TargetInput>): TargetInput => ({ kind: "none", mode: "", hashrateThs: "", preset: "", scalingStep: "", scalingMin: "", ...p });

describe("buildTargetChange", () => {
  it("one target kind, TH/s typed and sent as GH/s", () => {
    expect(buildTargetChange(input({ kind: "mode", mode: "low" })).change).toEqual({ tuning_mode: "low" });
    expect(buildTargetChange(input({ kind: "hashrate", hashrateThs: "1.05" })).change).toEqual({ tuning_hashrate_ghs: 1050 });
    expect(buildTargetChange(input({ kind: "preset", preset: "2500" })).change).toEqual({ tuning_preset: "2500" });
    expect(buildTargetChange(input({ scalingStep: "2", scalingMin: "60" })).change).toEqual({ scaling_step: 2, scaling_min: 60 });
  });
  it("validates", () => {
    expect(buildTargetChange(input({})).errors).toEqual(["Choose a tuning target and/or scaling"]);
    expect(buildTargetChange(input({ kind: "mode" })).errors).toEqual(["Pick a mining mode"]);
    expect(buildTargetChange(input({ scalingStep: "2" })).errors).toEqual(["Scaling needs both a step and a minimum"]);
    expect(thsToGhs("1.2345", [])).toBeUndefined();
    expect(thsToGhs("90", [])).toBe(90000);
  });
});

describe("planTarget", () => {
  it("mode: supported miners included, others explained with paths tried", () => {
    const p = planTarget(input({ kind: "mode", mode: "low" }), rows, false);
    expect(p.rows.map((r) => r.included)).toEqual([true, false, false, true]);
    expect(p.rows[1].note).toContain("mining mode not supported; tried: asic-rs: skipped");
    expect(p.rows[3].note).toBe("via axeos-http (frequency stand-in) (dry run only until hardware-confirmed)");
  });
  it("High is never offered as allowed", () => {
    const p = planTarget(input({ kind: "mode", mode: "high" }), [antminer], false);
    expect(p.rows[0]).toMatchObject({ included: false, note: "mode High not allowed on Antminer S19" });
  });
  it("hashrate: limits per model", () => {
    const p = planTarget(input({ kind: "hashrate", hashrateThs: "200" }), [epic], false);
    expect(p.rows[0].note).toBe("200000 GH/s outside 50000–110000 GH/s");
    expect(planTarget(input({ kind: "hashrate", hashrateThs: "90" }), [epic], false).rows[0].included).toBe(true);
  });
  it("preset must be listed; common presets", () => {
    expect(planTarget(input({ kind: "preset", preset: "3250" }), [vnish], false).rows[0].note).toBe("preset 3250 not offered by this miner");
    expect(commonPresets(rows)).toEqual(["2500", "3000"]);
    expect(commonPresets([vnish, { ...vnish, ip: "x", presets: ["3000"] }])).toEqual(["3000"]);
  });
  it("live send leaves out unconfirmed paths; request only carries included miners", () => {
    const live = planTarget(input({ kind: "mode", mode: "low" }), rows, true);
    expect(live.rows.every((r) => !r.included)).toBe(true);
    expect(buildSettingsRequest(live, { dryRun: false, concurrency: 4, staggerMs: 1000, canarySize: 1, haltThreshold: 0.2 })).toBeNull();
    const dry = planTarget(input({ kind: "mode", mode: "low" }), rows, false);
    expect(buildSettingsRequest(dry, { dryRun: true, concurrency: 4, staggerMs: 1000, canarySize: 1, haltThreshold: 0.2 })?.ips).toEqual(["192.0.2.1", "192.0.2.4"]);
  });
});

describe("TuningTargetDialog", () => {
  it("opens with Dry run checked and lists modes / presets per miner", () => {
    const { body } = render(TuningTargetDialog, { props: { ips: rows.map((r) => r.ip), onclose: () => {}, initialSupport: rows } });
    const cb = body.match(/<input[^>]*data-testid="tm-target-dry-run"[^>]*>/)?.[0] ?? "";
    expect(cb).toMatch(/\schecked(\s|=|>|\/)/);
    expect(body).toMatch(/>\s*Dry run\s*<\/button>/);
    expect(body).toContain("Low, Normal");
    expect(body).toContain("2500, 3000");
    expect(body).toContain("High mode is not allowed");
    expect(body.match(/data-testid="tm-target-row"/g)?.length).toBe(4);
  });
  it("button is disabled without a selection", () => {
    const { body } = render(TuningTargetButton, { props: { selectedIps: [] } });
    expect(body).toMatch(/data-testid="tm-target-open"[^>]*disabled|disabled[^>]*data-testid="tm-target-open"/);
  });
});

describe("rollback alert describes tuning targets", () => {
  it("before → attempted → after for a mode change rolled back", () => {
    expect(describeChange({ before: { tuning_mode: "normal" }, attempted: { tuning_mode: "low" }, after: { tuning_mode: "normal" } })).toBe(
      "normal → low → normal mode",
    );
    expect(describeChange({ before: { scaling_step: 5 }, attempted: { scaling_step: 2 }, after: { scaling_step: 5 } })).toBe("5 → 2 → 5 scaling step");
  });
});
