import type { ActionRowEvent } from "./types";

export type RunSummary = { kind: "dry" | "real"; text: string };

/**
 * Toolbar summary for the last finished run. Dry runs are reported as what
 * *would* have been sent, so "0/5 ok" never appears for a successful dry run.
 */
export function summarizeRun(results: ReadonlyArray<Pick<ActionRowEvent, "status">>): RunSummary {
  const total = results.length;
  const dry = results.filter((r) => r.status === "dryrun").length;
  if (total > 0 && dry > 0 && results.every((r) => r.status === "dryrun" || r.status === "unsupported" || r.status === "notfound" || r.status === "refused")) {
    return { kind: "dry", text: `Dry run: ${dry}/${total} would be sent (nothing sent)` };
  }
  const ok = results.filter((r) => r.status === "verified" || r.status === "ok").length;
  return { kind: "real", text: `Last run: ${ok}/${total} ok` };
}
