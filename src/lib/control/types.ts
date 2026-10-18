export type MinerAction = "start" | "stop" | "reboot" | "light_on" | "light_off";

/** Every action, in toolbar order. */
export const ALL_ACTIONS: readonly MinerAction[] = ["start", "stop", "reboot", "light_on", "light_off"];

/** Actions whose effect shows up in hashrate (post-run re-poll applies). */
export function affectsHashing(action: MinerAction): boolean {
  return action === "start" || action === "stop" || action === "reboot";
}

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
  light_on?: boolean;
  light_off?: boolean;
  routes?: Partial<Record<MinerAction, Route>>;
  /** Routes not yet confirmed on real hardware: dry run only. */
  unconfirmed?: MinerAction[];
};

export type CapsRow = {
  ip: string;
  found: boolean;
  caps: Capabilities;
  /** Per unsupported action: every path tried and why it was not usable. */
  why?: Partial<Record<MinerAction, string>>;
};

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
  light_on: "Light on",
  light_off: "Light off",
};

/** Above this many miners the dialog requires typing the action name. */
export const TYPED_CONFIRM_THRESHOLD = 5;

export function unsupportedReason(row: CapsRow | undefined, action: MinerAction): string | null {
  if (!row) return null; // unknown yet: allow, backend re-checks
  if (!row.found) return "Miner not reachable or not recognised";
  if (row.caps[action]) return null;
  const why = row.why?.[action];
  if (action === "light_on" || action === "light_off") {
    return why ? `No fault-light path; tried: ${why}` : "No fault-light path for this firmware";
  }
  if (why) return `Not supported; tried: ${why}`;
  if (action === "reboot") return "Firmware exposes no reboot API";
  return "Firmware has no pause/resume API (e.g. AxeOS < 2.14, stock Antminer without sleep mode)";
}

/** True when the route exists but is not yet confirmed on real hardware. */
export function dryRunOnly(row: CapsRow | undefined, action: MinerAction): boolean {
  return !!row?.found && !!row.caps[action] && !!row.caps.unconfirmed?.includes(action);
}

export const DRY_RUN_ONLY_NOTE = "dry run only: route not yet confirmed on real hardware";

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
