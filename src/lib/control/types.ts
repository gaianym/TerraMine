export type MinerAction = "start" | "stop" | "reboot";

export type ActionStatus =
  | "queued"
  | "sent"
  | "ok"
  | "rebooting"
  | "refused"
  | "failed"
  | "unsupported"
  | "notfound"
  | "dryrun"
  | "cancelled"
  | "verifying"
  | "verified"
  | "unverified";

/** How an action reaches the miner: asic-rs natively, or an override. */
export type Route = "asic-rs" | "axeos" | "s9-cgi" | "mock";

/** Support flags plus the per-action route (absent = unsupported). */
export type Capabilities = {
  start: boolean;
  stop: boolean;
  reboot: boolean;
  routes?: Partial<Record<MinerAction, Route>>;
};

export type CapsRow = { ip: string; found: boolean; caps: Capabilities };

export type ActionRowEvent = {
  ip: string;
  action: MinerAction;
  status: ActionStatus;
  msg: string | null;
  /** Route the action was sent on, once the capability gate passed. */
  route?: Route;
};

export type DoneEvent = { run_id: string; results: ActionRowEvent[] };

export type ActionRequest = {
  ips: string[];
  action: MinerAction;
  stagger_ms: number;
  concurrency: number;
  timeout_sec: number;
  dry_run: boolean;
  verify: boolean;
  verify_timeout_sec: number;
};

export type FirmwareKind = "antminer-stock" | "braiins" | "luxos" | "vnish" | "whatsminer";

export type CredentialSummary = {
  firmware: FirmwareKind;
  username: string | null;
  has_password: boolean;
};

export const EVENT_ROW = "control:row";
export const EVENT_DONE = "control:done";

export const ACTION_LABEL: Record<MinerAction, string> = {
  start: "Start",
  stop: "Stop",
  reboot: "Reboot",
};

/** Above this many miners the dialog requires typing the action name. */
export const TYPED_CONFIRM_THRESHOLD = 5;

export function unsupportedReason(row: CapsRow | undefined, action: MinerAction): string | null {
  if (!row) return null; // unknown yet: allow, backend re-checks
  if (!row.found) return "Miner not reachable or not recognised";
  if (row.caps[action]) return null;
  if (action === "reboot") return "Firmware exposes no reboot API";
  return "Firmware has no pause/resume API (e.g. AxeOS < 2.14, stock Antminer without sleep mode)";
}

export function isTerminal(s: ActionStatus): boolean {
  return !["queued", "sent", "verifying"].includes(s);
}

export function statusTone(s: ActionStatus): "ok" | "warn" | "bad" | "busy" | "muted" {
  switch (s) {
    case "verified":
    case "ok":
      return "ok";
    case "rebooting":
    case "unverified":
    case "dryrun":
      return "warn";
    case "failed":
    case "refused":
    case "notfound":
      return "bad";
    case "queued":
    case "sent":
    case "verifying":
      return "busy";
    default:
      return "muted";
  }
}
