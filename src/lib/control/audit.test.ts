import { describe, expect, it } from "vitest";
import { brief, buildAuditFilter, dayBoundMs, emptyAuditFilter, formatLocal, outcomeTone } from "./audit";

describe("audit helpers", () => {
  it("builds a backend filter, dropping blanks", () => {
    const f = buildAuditFilter({ ...emptyAuditFilter(), ip: " 192.0.2.4 ", outcome: "refused", dryRun: "live" }, 200, 400);
    expect(f).toMatchObject({ ip: "192.0.2.4", action: null, outcome: "refused", phase: null, dry_run: false, text: null, limit: 200, offset: 400 });
    expect(buildAuditFilter(emptyAuditFilter(), 10).dry_run).toBeNull();
    expect(buildAuditFilter({ ...emptyAuditFilter(), dryRun: "dry" }, 10).dry_run).toBe(true);
  });

  it("uses local-day bounds", () => {
    const a = dayBoundMs("2026-10-20", false)!;
    const b = dayBoundMs("2026-10-20", true)!;
    expect(b - a).toBe(86_400_000 - 1);
    expect(new Date(a).getHours()).toBe(0);
    expect(dayBoundMs("nope", false)).toBeNull();
  });

  it("formats with a zone label", () => {
    const s = formatLocal(Date.UTC(2026, 9, 20, 15, 49, 3), "America/Chicago");
    expect(s).toContain("10:49:03");
    expect(s).toContain("CDT");
  });

  it("tones outcomes and truncates details", () => {
    expect(outcomeTone("verified")).toBe("ok");
    expect(outcomeTone("dryrun")).toBe("info");
    expect(outcomeTone("no_path")).toBe("warn");
    expect(outcomeTone("failed")).toBe("bad");
    expect(brief({ a: 1 })).toBe('{"a":1}');
    expect(brief("x".repeat(300), 10)).toBe("xxxxxxxxxx…");
    expect(brief(null)).toBe("");
  });
});
