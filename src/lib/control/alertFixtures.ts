import type { JobAlert } from "./alertBanner";

/** Test fixture: a 4-miner job with 3 rollbacks. */
export function sampleAlert(over: Partial<JobAlert> = {}): JobAlert {
  return {
    run_id: "job-1",
    total: 4,
    rolled_back: 3,
    rollback_failed: 0,
    halted: 0,
    headline: "3 of 4 rolled back",
    likely_external: false,
    external_note: null,
    groups: [
      {
        cause: "overheat",
        label: "overheat",
        count: 1,
        miners: [
          {
            ip: "192.0.2.2",
            status: "rolled_back",
            before: { frequency_mhz: 525 },
            attempted: { frequency_mhz: 600 },
            after: { frequency_mhz: 525 },
            sample: { reachable: true, hashrate_ghs: 1180.5, temp_c: 95 },
            trigger: null,
            detail: null,
            likely_external: false,
          },
        ],
      },
    ],
    halted_ips: [],
    retry: { ips: ["192.0.2.2", "192.0.2.3", "192.0.2.4"], change: { frequency_mhz: 600 }, dry_run: true },
    ...over,
  };
}
