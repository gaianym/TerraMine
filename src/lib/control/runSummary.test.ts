import { describe, expect, it } from "vitest";
import { summarizeRun } from "./runSummary";

describe("summarizeRun (demo bug: dry run read as 'Last run: 0/5 ok')", () => {
  it("reports a dry run as would-be-sent, never as 0/N ok", () => {
    const s = summarizeRun(Array.from({ length: 5 }, () => ({ status: "dryrun" as const })));
    expect(s.kind).toBe("dry");
    expect(s.text).toBe("Dry run: 5/5 would be sent (nothing sent)");
    expect(s.text).not.toMatch(/Last run/);
    expect(s.text).not.toMatch(/0\/5/);
  });
  it("counts skipped miners in a dry run", () => {
    expect(summarizeRun([{ status: "dryrun" }, { status: "unsupported" }]).text).toBe(
      "Dry run: 1/2 would be sent (nothing sent)",
    );
  });
  it("keeps the real-run summary", () => {
    const s = summarizeRun([{ status: "verified" }, { status: "ok" }, { status: "failed" }]);
    expect(s).toEqual({ kind: "real", text: "Last run: 2/3 ok" });
  });
});
