/**
 * SSH fallbacks and pool divert (plan K2): pure helpers for the dialog.
 * The backend re-checks everything (allow-list, firmware gate, host-key
 * pin, keychain login, typed confirmation, hardware-confirmed gate); this
 * only gives the operator an honest per-miner plan before sending.
 */

export const EVENT_SSH_ROW = "ssh:row";
export const EVENT_SSH_DONE = "ssh:done";
export const EVENT_DIVERT_ROW = "divert:row";
export const EVENT_DIVERT_DONE = "divert:done";

export type SshAction = "restart" | "read_logs" | "grant_api_write";
export type FallbackChoice = SshAction | "divert" | "restore";
export const GRANT_CONFIRM_TEXT = "ssh_grant_api_write";

export const FALLBACK_LABELS: Record<FallbackChoice, string> = {
  restart: "Restart mining process (SSH)",
  read_logs: "Read logs (SSH)",
  grant_api_write: "Grant API write — stock S9 (SSH)",
  divert: "Stop by pool divert",
  restore: "Start: restore pools after divert",
};

export type SshStatus =
  | "dry_run" | "ok" | "verified" | "unverified" | "rolled_back" | "rollback_failed" | "refused" | "failed"
  | "unsupported" | "no_credentials" | "host_key_unpinned" | "host_key_changed";

export type SshActionSupport = { action: SshAction; supported: boolean; reason: string | null; hw_confirmed: boolean };
export type SshAccessSummary = { username: string | null; auth: "password" | "private_key" | null; pinned_fingerprint: string | null };
export type SshSupportRow = {
  ip: string;
  found: boolean;
  class: string | null;
  model: string | null;
  actions: SshActionSupport[];
  access: SshAccessSummary | null;
  native_stop: boolean;
  native_start: boolean;
  diverted: boolean;
};

export type SshOutcome = {
  ip: string;
  action: SshAction;
  status: SshStatus;
  msg: string;
  plan: string[];
  paths_tried: string[];
  fingerprint: string | null;
  output: string | null;
  backup: string | null;
};

export type DivertOutcome = {
  ip: string;
  op: "divert" | "restore";
  status: "dry_run" | "refused" | "no_path" | "failed" | "verified" | "rolled_back" | "rollback_failed" | "skipped";
  msg: string | null;
  paths_tried: string[];
};

export type HostKeyInfo = { ip: string; fingerprint: string; pinned: string | null; matches: boolean };

export type PlanRow = { ip: string; included: boolean; note: string };
export type FallbackPlan = { errors: string[]; rows: PlanRow[] };

/** a.b.c.d/len, as the backend checks it. */
export function cidrError(cidr: string): string | null {
  const t = cidr.trim();
  const m = t.match(/^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\/(\d{1,2})$/);
  if (!m) return "LAN range must look like a.b.c.0/24";
  if (m.slice(1, 5).some((o) => Number(o) > 255) || Number(m[5]) > 32) return "LAN range is out of bounds";
  if (Number(m[5]) === 0) return "0/0 would give the whole internet write access; use your LAN range";
  return null;
}

export function planFallback(choice: FallbackChoice, support: SshSupportRow[], live: boolean, cidr = ""): FallbackPlan {
  const errors: string[] = [];
  if (choice === "grant_api_write") {
    const e = cidrError(cidr);
    if (e) errors.push(e);
    if (live && support.filter((r) => r.found).length !== 1) errors.push("Grant API write runs on exactly one miner at a time");
  }
  const rows = support.map((r): PlanRow => {
    if (!r.found) return { ip: r.ip, included: false, note: "not discovered yet" };
    if (choice === "divert" || choice === "restore") {
      if (choice === "divert" && r.native_stop) return { ip: r.ip, included: false, note: "has a native Stop; use Stop instead" };
      if (choice === "restore" && r.native_start && !r.diverted) return { ip: r.ip, included: false, note: "has a native Start; use Start instead" };
      if (choice === "divert" && r.diverted) return { ip: r.ip, included: false, note: "already diverted; restore first" };
      if (choice === "restore" && !r.diverted) return { ip: r.ip, included: false, note: "no divert on record" };
      return { ip: r.ip, included: true, note: choice === "divert" ? "save pools, switch to a dead local pool" : "write the saved pools back" };
    }
    const a = r.actions.find((x) => x.action === choice);
    if (!a?.supported) return { ip: r.ip, included: false, note: a?.reason ?? "not supported" };
    const notes: string[] = [];
    if (!a.hw_confirmed) notes.push("dry run only until confirmed on hardware");
    if (!r.access?.username) notes.push("no SSH login on file");
    notes.push(r.access?.pinned_fingerprint ? "host key pinned" : "host key not pinned yet");
    return { ip: r.ip, included: true, note: notes.join("; ") };
  });
  return { errors, rows };
}

export function buildSshJob(choice: SshAction, ips: string[], o: { dryRun: boolean; typed: string; cidr: string }) {
  const grant = choice === "grant_api_write";
  return {
    ips,
    action: choice,
    dry_run: o.dryRun,
    typed: grant && !o.dryRun ? o.typed.trim() : null,
    allow_irreversible: grant && !o.dryRun,
    cidr: grant ? o.cidr.trim() : null,
  };
}

export function buildDivertJob(op: "divert" | "restore", ips: string[], dryRun: boolean) {
  return { ips, op, dry_run: dryRun, confirm: { allow_pool_change: !dryRun } };
}

/** A first-use outcome: show the trust prompt with this fingerprint. */
export function trustPrompt(o: SshOutcome): { ip: string; fingerprint: string } | null {
  return o.status === "host_key_unpinned" && o.fingerprint ? { ip: o.ip, fingerprint: o.fingerprint } : null;
}

export function statusTone(s: string): "ok" | "warn" | "bad" | "info" {
  if (["ok", "verified"].includes(s)) return "ok";
  if (["dry_run", "unverified", "host_key_unpinned", "no_credentials", "unsupported", "no_path", "skipped"].includes(s)) return s === "dry_run" ? "info" : "warn";
  return "bad";
}
