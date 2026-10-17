import { describe, expect, it } from "vitest";
import { isSettled, refreshUntilSettled, type RowSnapshot } from "./postActionRefresh";

/** Fake clock + scripted poll results per IP. */
function harness(script: Record<string, RowSnapshot[]>) {
  let t = 0;
  const polls: string[][] = [];
  const idx: Record<string, number> = {};
  const current: Record<string, RowSnapshot | undefined> = {};
  const states: [string, string][] = [];
  return {
    polls,
    states,
    deps: {
      enrich: async (ips: string[]) => {
        polls.push([...ips]);
        for (const ip of ips) {
          const i = idx[ip] ?? 0;
          const seq = script[ip];
          current[ip] = seq[Math.min(i, seq.length - 1)];
          idx[ip] = i + 1;
        }
      },
      snapshot: (ip: string) => current[ip],
      sleep: async (ms: number) => {
        t += ms;
      },
      now: () => t,
      onState: (ip: string, s: string) => states.push([ip, s]),
    },
  };
}

const zero: RowSnapshot = { reachable: true, hashrateHs: 0 };
const down: RowSnapshot = { reachable: false, hashrateHs: null };
const hashing: RowSnapshot = { reachable: true, hashrateHs: 1.1e12 };

describe("isSettled", () => {
  it("does not treat 0 H/s after reboot as back online", () => {
    expect(isSettled("reboot", zero)).toBe(false);
    expect(isSettled("reboot", down)).toBe(false);
    expect(isSettled("reboot", hashing)).toBe(true);
    expect(isSettled("start", zero)).toBe(false);
    expect(isSettled("stop", zero)).toBe(true);
    expect(isSettled("reboot", undefined)).toBe(false);
  });
});

describe("refreshUntilSettled (demo bug: row stale at 0 H/s after reboot)", () => {
  it("keeps re-polling a rebooted miner until it is hashing, then stops", async () => {
    const h = harness({ "192.0.2.10": [down, zero, zero, hashing] });
    const res = await refreshUntilSettled([{ ip: "192.0.2.10", action: "reboot" }], h.deps, {
      intervalMs: 10_000,
      timeoutMs: 300_000,
    });
    expect(res).toEqual({ "192.0.2.10": "online" });
    expect(h.polls.length).toBe(4);
    expect(h.states.at(0)).toEqual(["192.0.2.10", "polling"]);
    expect(h.states.at(-1)).toEqual(["192.0.2.10", "online"]);
  });

  it("only re-polls miners that are still pending", async () => {
    const h = harness({ "192.0.2.1": [hashing], "192.0.2.2": [zero, hashing] });
    await refreshUntilSettled(
      [
        { ip: "192.0.2.1", action: "reboot" },
        { ip: "192.0.2.2", action: "reboot" },
      ],
      h.deps,
      { intervalMs: 1000, timeoutMs: 60_000 },
    );
    expect(h.polls).toEqual([["192.0.2.1", "192.0.2.2"], ["192.0.2.2"]]);
  });

  it("gives up with timeout instead of polling forever", async () => {
    const h = harness({ "192.0.2.3": [zero] });
    const res = await refreshUntilSettled([{ ip: "192.0.2.3", action: "reboot" }], h.deps, {
      intervalMs: 10_000,
      timeoutMs: 30_000,
    });
    expect(res).toEqual({ "192.0.2.3": "timeout" });
    expect(h.polls.length).toBe(4);
  });

  it("refreshes a stopped miner once and reports paused", async () => {
    const h = harness({ "192.0.2.4": [zero] });
    const res = await refreshUntilSettled([{ ip: "192.0.2.4", action: "stop" }], h.deps);
    expect(res).toEqual({ "192.0.2.4": "paused" });
    expect(h.polls.length).toBe(1);
  });
});
