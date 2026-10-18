import type { ActionRowEvent, ActionStatus } from "./types";

/** Human-readable per-row action status for the Control column. */
export function formatActionStatusLabel(row: Pick<ActionRowEvent, "action" | "status">): string {
  const { action, status } = row;
  switch (status) {
    case "queued":
      return "queued";
    case "sent":
      return "sent";
    case "verifying":
      return action === "reboot" ? "rebooting…" : "verifying…";
    case "rebooting":
      return "rebooting…";
    case "verified":
      if (action === "reboot") return "back online";
      if (action === "stop") return "verified paused";
      if (action === "start") return "verified running";
      if (action === "light_on") return "light on";
      if (action === "light_off") return "light off";
      return "verified";
    case "ok":
      if (action === "stop") return "paused";
      if (action === "start") return "running";
      if (action === "light_on") return "light on (sent)";
      if (action === "light_off") return "light off (sent)";
      return "ok";
    case "unverified":
      return "unverified";
    case "failed":
      return "failed";
    case "refused":
      return "refused";
    case "unsupported":
      return "unsupported";
    case "notfound":
      return "not found";
    case "dryrun":
      return "dry run";
    case "cancelled":
      return "cancelled";
    default:
      return status;
  }
}

/** IPs whose discovery row should be re-polled after a control run finishes. */
export function ipsNeedingRefresh(
  results: ReadonlyArray<{ ip: string; status: ActionStatus }>,
): string[] {
  const out: string[] = [];
  for (const r of results) {
    if (
      r.status === "verified" ||
      r.status === "ok" ||
      r.status === "unverified" ||
      r.status === "rebooting"
    ) {
      out.push(r.ip);
    }
  }
  return [...new Set(out)];
}

export function isBusyStatus(status: ActionStatus): boolean {
  return status === "queued" || status === "sent" || status === "verifying" || status === "rebooting";
}

/** AG Grid community modules TerraMine must register for grid features we use. */
export const REQUIRED_AG_GRID_MODULE_IDS = [
  "ClientSideRowModelModule",
  "ColumnApiModule",
  "RowSelectionModule",
] as const;

export function missingAgGridModules(registeredNames: readonly string[]): string[] {
  return REQUIRED_AG_GRID_MODULE_IDS.filter((id) => !registeredNames.includes(id));
}
