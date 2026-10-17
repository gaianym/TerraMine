import type { MinerAction } from "./types";

export type RowSnapshot = {
  /** Row present in the latest poll (miner answered). */
  reachable: boolean;
  /** Parsed hashrate in H/s, null if unknown. */
  hashrateHs: number | null;
};

export type RefreshTarget = { ip: string; action: MinerAction };
export type RefreshState = "polling" | "online" | "paused" | "timeout";

/**
 * Is the row's latest poll consistent with the action having taken effect?
 * Reboot/start: miner answers AND is hashing (>0 H/s). A miner that just came
 * back from reboot reports 0 H/s for a while; that is NOT back online yet.
 * Stop: miner answers (hashrate may still be decaying); one fresh poll is enough.
 */
export function isSettled(action: MinerAction, snap: RowSnapshot | undefined): boolean {
  if (!snap || !snap.reachable) return false;
  if (action === "stop") return true;
  return snap.hashrateHs !== null && snap.hashrateHs > 0;
}

export type RefreshDeps = {
  /** Re-poll these IPs and resolve once the grid rows have been updated. */
  enrich: (ips: string[]) => Promise<void>;
  snapshot: (ip: string) => RowSnapshot | undefined;
  sleep: (ms: number) => Promise<void>;
  now: () => number;
  onState?: (ip: string, state: RefreshState) => void;
  isCancelled?: () => boolean;
};

export type RefreshOptions = { intervalMs: number; timeoutMs: number };
export const DEFAULT_REFRESH_OPTIONS: RefreshOptions = { intervalMs: 10_000, timeoutMs: 300_000 };

/**
 * Keep re-polling targets until each one is settled (see isSettled) or the
 * timeout elapses. Always polls at least once, so the row is refreshed even
 * when the miner was already settled.
 */
export async function refreshUntilSettled(
  targets: ReadonlyArray<RefreshTarget>,
  deps: RefreshDeps,
  opts: RefreshOptions = DEFAULT_REFRESH_OPTIONS,
): Promise<Record<string, RefreshState>> {
  const byIp = new Map<string, MinerAction>();
  for (const t of targets) byIp.set(t.ip, t.action);
  const result: Record<string, RefreshState> = {};
  let pending = [...byIp.keys()];
  for (const ip of pending) deps.onState?.(ip, "polling");
  const deadline = deps.now() + opts.timeoutMs;
  let first = true;
  while (pending.length) {
    if (deps.isCancelled?.()) break;
    if (!first) await deps.sleep(opts.intervalMs);
    first = false;
    await deps.enrich(pending);
    const still: string[] = [];
    for (const ip of pending) {
      const action = byIp.get(ip)!;
      if (isSettled(action, deps.snapshot(ip))) {
        const st: RefreshState = action === "stop" ? "paused" : "online";
        result[ip] = st;
        deps.onState?.(ip, st);
      } else {
        still.push(ip);
      }
    }
    pending = still;
    if (pending.length && deps.now() >= deadline) break;
  }
  for (const ip of pending) {
    result[ip] = "timeout";
    deps.onState?.(ip, "timeout");
  }
  return result;
}
