/** Audit log viewer (plan step I): types and pure helpers. */

export type AuditRecord = {
  ts_ms: number;
  run_id: string;
  ip: string;
  action: string;
  phase: string;
  dry_run: boolean;
  irreversible: boolean;
  device_class: string | null;
  model: string | null;
  firmware: string | null;
  requested: unknown;
  snapshot_id: string | null;
  pre_state: unknown;
  paths_tried: string[];
  outcome: string;
  msg: string | null;
};

export type AuditPage = {
  records: AuditRecord[];
  total: number;
  skipped_lines: number;
  ips: string[];
  actions: string[];
  outcomes: string[];
};

export type AuditFilterInput = {
  ip: string;
  action: string;
  outcome: string;
  phase: "" | "attempt" | "outcome";
  dryRun: "" | "dry" | "live";
  text: string;
  /** `YYYY-MM-DD` in the operator's local zone. */
  since: string;
  until: string;
};

export const emptyAuditFilter = (): AuditFilterInput => ({ ip: "", action: "", outcome: "", phase: "", dryRun: "", text: "", since: "", until: "" });

/** Local-midnight bounds; `until` covers its whole day. */
export function dayBoundMs(day: string, end: boolean): number | null {
  const m = day.match(/^(\d{4})-(\d{2})-(\d{2})$/);
  if (!m) return null;
  const d = new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]));
  return end ? d.getTime() + 86_400_000 - 1 : d.getTime();
}

export function buildAuditFilter(f: AuditFilterInput, limit: number, offset = 0) {
  const s = (v: string) => (v.trim() ? v.trim() : null);
  return {
    ip: s(f.ip),
    action: s(f.action),
    outcome: s(f.outcome),
    phase: s(f.phase),
    dry_run: f.dryRun === "" ? null : f.dryRun === "dry",
    text: s(f.text),
    since_ms: dayBoundMs(f.since, false),
    until_ms: dayBoundMs(f.until, true),
    limit,
    offset,
  };
}

/** Local time with the zone's short name, e.g. "Oct 20, 10:49:03 AM CDT". */
export function formatLocal(ms: number, timeZone?: string): string {
  return new Intl.DateTimeFormat("en-US", {
    month: "short", day: "numeric", hour: "numeric", minute: "2-digit", second: "2-digit", timeZoneName: "short", timeZone,
  }).format(new Date(ms));
}

export function outcomeTone(o: string): "ok" | "warn" | "bad" | "info" {
  if (["ok", "verified", "sent"].includes(o)) return "ok";
  if (["dry_run", "dryrun", "pending"].includes(o)) return "info";
  if (["refused", "no_path", "unsupported", "skipped", "unverified", "cancelled", "rolled_back"].includes(o)) return "warn";
  return "bad";
}

/** Compact JSON for the detail row. The backend has already redacted it. */
export function brief(v: unknown, max = 240): string {
  if (v === null || v === undefined) return "";
  const s = typeof v === "string" ? v : JSON.stringify(v);
  return s.length > max ? `${s.slice(0, max)}…` : s;
}
