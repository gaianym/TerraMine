import { formatActionStatusLabel } from "./statusLabel";
import type { RefreshState } from "./postActionRefresh";
import { statusTone, type ActionRowEvent } from "./types";

export type ChipTone = "ok" | "warn" | "bad" | "busy" | "muted" | "dry";

export const CHIP_CLASS: Record<ChipTone, string> = {
  ok: "tm-chip-ok",
  warn: "tm-chip-warn",
  bad: "tm-chip-bad",
  busy: "tm-chip-busy",
  muted: "tm-chip-muted",
  dry: "tm-chip-dry",
};

export type ChipSpec = { className: string; text: string; title: string; tone: ChipTone };

/**
 * Per-row status badge. Combines the control-run status with the post-action
 * re-poll state so the badge only says "back online" once the row shows the
 * miner hashing again.
 */
export function statusChipSpec(
  row: Pick<ActionRowEvent, "action" | "status" | "msg">,
  refresh?: RefreshState,
): ChipSpec {
  let tone: ChipTone = row.status === "dryrun" ? "dry" : statusTone(row.status);
  let text = formatActionStatusLabel(row);
  const realOutcome = row.status === "verified" || row.status === "ok" || row.status === "unverified" || row.status === "rebooting";
  if (realOutcome && refresh) {
    if (refresh === "polling") {
      tone = "busy";
      text = row.action === "reboot" ? "waiting for hashrate…" : "refreshing…";
    } else if (refresh === "online") {
      tone = "ok";
      text = row.action === "reboot" ? "back online" : "verified running";
    } else if (refresh === "paused") {
      tone = "ok";
      text = "verified paused";
    } else if (refresh === "timeout") {
      tone = "bad";
      text = row.action === "reboot" ? "not back (timeout)" : "refresh timed out";
    }
  }
  return {
    tone,
    text,
    title: row.msg ?? "",
    className: `tm-chip ${CHIP_CLASS[tone]}`,
  };
}
