import { render } from "svelte/server";
import { describe, expect, it, vi } from "vitest";
import { describeChange } from "./alertBanner";
import { buildSettingsRequest, formatRange, planPower, type SettingsSupportRow } from "./power";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => []) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

const sup = (path: string | null, confirmed = true, tried: string[] = []) => ({ path, confirmed, tried });

const vnish: SettingsSupportRow = {
  ip: "192.0.2.1", found: true, class: "vnish", model: "Antminer S19",
  power_w: [1000, 3250], tuning_pct: [50, 100],
  power: sup("asic-rs"), tuning: sup("asic-rs"),
};
const braiins: SettingsSupportRow = {
  ip: "192.0.2.2", found: true, class: "braiins_os", model: "Antminer S19",
  power_w: [1000, 3250], tuning_pct: null,
  power: sup("asic-rs", false), tuning: sup(null, false, ["asic-rs: supports_set_tuning_percent() = false"]),
};
const bitaxe: SettingsSupportRow = {
  ip: "192.0.2.3", found: true, class: "axeos", model: "BM1370",
  power_w: null, tuning_pct: null,
  power: sup(null, false, ["asic-rs: supports_set_power_limit() = false", "axeos-http: power_limit_w is not writable over AxeOS"]),
  tuning: sup(null, false, ["asic-rs: supports_set_tuning_percent() = false"]),
};
const gone: SettingsSupportRow = { ...bitaxe, ip: "192.0.2.4", found: false, power: sup(null), tuning: sup(null) };
const rows = [vnish, braiins, bitaxe, gone];

describe("planPower", () => {
  it("validates typed values", () => {
    expect(planPower({ powerW: "", tuningPct: "" }, rows, false).errors).toEqual(["Enter a power limit and/or a tuning percent"]);
    expect(planPower({ powerW: "25.5", tuningPct: "" }, rows, false).errors).toEqual(["Power limit must be a whole number"]);
    expect(planPower({ powerW: "-1", tuningPct: "" }, rows, false).errors).toEqual(["Power limit must be a whole number"]);
    expect(planPower({ powerW: "", tuningPct: "120" }, rows, false).errors).toEqual(["Tuning percent cannot exceed 100"]);
  });

  it("dry run: includes in-limit supported miners, explains the rest", () => {
    const p = planPower({ powerW: "2500", tuningPct: "" }, rows, false);
    expect(p.change).toEqual({ power_limit_w: 2500 });
    expect(p.rows.map((r) => r.included)).toEqual([true, true, false, false]);
    expect(p.rows[0].note).toBe("via asic-rs");
    expect(p.rows[1].note).toMatch(/dry run only until hardware-confirmed/);
    expect(p.rows[2].note).toMatch(/^power limit not supported; tried: asic-rs: supports_set_power_limit\(\) = false; axeos-http/);
    expect(p.rows[3].note).toBe("not reachable or not recognised");
  });

  it("live: leaves out paths not confirmed on hardware", () => {
    const p = planPower({ powerW: "2500", tuningPct: "" }, rows, true);
    expect(p.rows.map((r) => r.included)).toEqual([true, false, false, false]);
    expect(p.rows[1].note).toMatch(/dry run only \(not yet confirmed on real hardware\)/);
  });

  it("refuses out-of-limit values per model", () => {
    const p = planPower({ powerW: "4000", tuningPct: "" }, rows, false);
    expect(p.rows[0]).toMatchObject({ included: false, note: "4000 W outside 1000–3250 W" });
    const t = planPower({ powerW: "", tuningPct: "30" }, rows, false);
    expect(t.rows[0]).toMatchObject({ included: false, note: "30 % outside 50–100 %" });
  });

  it("tuning percent only where supported", () => {
    const p = planPower({ powerW: "2500", tuningPct: "80" }, rows, false);
    expect(p.change).toEqual({ power_limit_w: 2500, tuning_pct: 80 });
    expect(p.rows[0].included).toBe(true);
    expect(p.rows[1]).toMatchObject({ included: false });
    expect(p.rows[1].note).toMatch(/^tuning percent not supported/);
  });
});

describe("buildSettingsRequest", () => {
  const opts = { dryRun: true, concurrency: 4, staggerMs: 1000, canarySize: 5, haltThreshold: 0.2 };
  it("sends only included miners, dry run as given, canary clamped", () => {
    const req = buildSettingsRequest(planPower({ powerW: "2500", tuningPct: "" }, rows, false), opts);
    expect(req).toEqual({
      ips: ["192.0.2.1", "192.0.2.2"], change: { power_limit_w: 2500 }, dry_run: true,
      concurrency: 4, stagger_ms: 1000, canary_size: 2, halt_threshold: 0.2,
    });
  });
  it("nothing to send on errors or no eligible miner", () => {
    expect(buildSettingsRequest(planPower({ powerW: "x", tuningPct: "" }, rows, false), opts)).toBeNull();
    expect(buildSettingsRequest(planPower({ powerW: "2500", tuningPct: "" }, [bitaxe], false), opts)).toBeNull();
  });
});

describe("display helpers", () => {
  it("formatRange", () => {
    expect(formatRange([1000, 3250], "W")).toBe("1000–3250 W");
    expect(formatRange(null, "W")).toBe("no limit set (write refused)");
  });
  it("alert describes tuning percent changes", () => {
    expect(describeChange({ before: { tuning_pct: 100 }, attempted: { tuning_pct: 80 }, after: { tuning_pct: 100 } })).toBe(
      "100 → 80 → 100 % tuning",
    );
  });
});

describe("PowerTuningDialog", async () => {
  const { default: PowerTuningDialog } = await import("./PowerTuningDialog.svelte");
  const { default: PowerTuningButton } = await import("./PowerTuningButton.svelte");
  it("opens with Dry run checked and shows limits / not supported per miner", () => {
    const { body } = render(PowerTuningDialog, { props: { ips: rows.map((r) => r.ip), onclose: () => {}, initialSupport: rows } });
    const cb = body.match(/<input[^>]*data-testid="tm-power-dry-run"[^>]*>/)?.[0] ?? "";
    expect(cb).toMatch(/\schecked(\s|=|>|\/)/);
    expect(body).toMatch(/>\s*Dry run\s*<\/button>/);
    expect(body).toContain("1000–3250 W");
    expect(body).toContain("50–100 %");
    expect(body).toContain("not supported");
    expect(body.match(/data-testid="tm-power-row"/g)?.length).toBe(4);
  });
  it("button disabled with no selection", () => {
    const { body } = render(PowerTuningButton, { props: { selectedIps: [] } });
    expect(body).toMatch(/<button[^>]*disabled/);
  });
});
