/**
 * Capability matrix (plan step I): types and pure helpers. Every cell comes
 * from the backend's capability gates (driver caps, the safety engine's
 * path selection, asic-rs supports_* flags); nothing here decides support.
 */

export type CellStatus = "confirmed" | "dry_run_only" | "unsupported";
export type MatrixCell = {
  action: string;
  group: "control" | "settings";
  label: string;
  status: CellStatus;
  path: string | null;
  paths_tried: string[];
};
export type NativeFlag = { name: string; supported: boolean };
export type MatrixRow = {
  ip: string;
  found: boolean;
  class: string | null;
  make: string | null;
  model: string | null;
  firmware: string | null;
  driver: string | null;
  cells: MatrixCell[];
  native: NativeFlag[];
  error: string | null;
};

export const STATUS_TEXT: Record<CellStatus | "mixed", string> = {
  confirmed: "Live",
  dry_run_only: "Dry run only",
  unsupported: "Not supported",
  mixed: "Mixed",
};

/** Column order, taken from the first found row (backend order). */
export function columns(rows: MatrixRow[]): Array<Pick<MatrixCell, "action" | "label" | "group">> {
  const r = rows.find((x) => x.found && x.cells.length);
  return r ? r.cells.map(({ action, label, group }) => ({ action, label, group })) : [];
}

export type GroupCell = {
  action: string;
  status: CellStatus | "mixed";
  counts: Record<CellStatus, number>;
  /** Distinct paths seen across the group. */
  paths: string[];
  /** Per miner, for the paths-tried drawer. */
  members: Array<{ ip: string; cell: MatrixCell }>;
};

export type FirmwareGroup = { key: string; class: string; model: string; firmware: string; ips: string[]; cells: Record<string, GroupCell> };

export function firmwareKey(r: MatrixRow): string {
  return `${r.class ?? "unknown"} | ${r.model ?? "?"} | ${r.firmware ?? "?"}`;
}

/** Per-firmware view: miners with the same class, model and firmware. */
export function groupByFirmware(rows: MatrixRow[]): FirmwareGroup[] {
  const groups = new Map<string, FirmwareGroup>();
  for (const r of rows.filter((x) => x.found)) {
    const key = firmwareKey(r);
    let g = groups.get(key);
    if (!g) {
      g = { key, class: r.class ?? "unknown", model: r.model ?? "?", firmware: r.firmware ?? "?", ips: [], cells: {} };
      groups.set(key, g);
    }
    g.ips.push(r.ip);
    for (const c of r.cells) {
      const gc = (g.cells[c.action] ??= { action: c.action, status: c.status, counts: { confirmed: 0, dry_run_only: 0, unsupported: 0 }, paths: [], members: [] });
      gc.counts[c.status] += 1;
      gc.members.push({ ip: r.ip, cell: c });
      if (c.path && !gc.paths.includes(c.path)) gc.paths.push(c.path);
      if (gc.status !== c.status) gc.status = "mixed";
    }
  }
  return [...groups.values()].sort((a, b) => a.key.localeCompare(b.key));
}

/** Totals for the header: how many miner×action cells are live / dry / none. */
export function summarize(rows: MatrixRow[]): Record<CellStatus, number> & { miners: number; missing: number } {
  const out = { confirmed: 0, dry_run_only: 0, unsupported: 0, miners: 0, missing: 0 };
  for (const r of rows) {
    if (!r.found) {
      out.missing += 1;
      continue;
    }
    out.miners += 1;
    for (const c of r.cells) out[c.status] += 1;
  }
  return out;
}

export function cellClass(s: CellStatus | "mixed"): string {
  switch (s) {
    case "confirmed":
      return "bg-emerald-900/50 text-emerald-300";
    case "dry_run_only":
      return "bg-amber-900/40 text-amber-200";
    case "mixed":
      return "bg-sky-900/40 text-sky-200";
    default:
      return "text-zinc-500";
  }
}

export function cellGlyph(s: CellStatus | "mixed"): string {
  return s === "confirmed" ? "✓ live" : s === "dry_run_only" ? "dry run" : s === "mixed" ? "mixed" : "—";
}
