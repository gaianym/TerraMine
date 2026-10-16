import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  EVENT_DONE,
  EVENT_ROW,
  type ActionRequest,
  type ActionRowEvent,
  type CapsRow,
  type DoneEvent,
  type MinerAction,
} from "./types";

/** Shared, reactive state for miner control actions. */
class ControlState {
  capsByIp = $state<Record<string, CapsRow>>({});
  rowByIp = $state<Record<string, ActionRowEvent>>({});
  activeRunId = $state<string | null>(null);
  lastDone = $state<DoneEvent | null>(null);
  log = $state<ActionRowEvent[]>([]);
  error = $state<string | null>(null);
  /** Bumps on every row event so grids can refresh cells. */
  version = $state(0);
  private unlisten: UnlistenFn[] = [];

  async attach() {
    if (this.unlisten.length) return;
    this.unlisten.push(
      await listen<ActionRowEvent>(EVENT_ROW, (e) => {
        this.rowByIp[e.payload.ip] = e.payload;
        this.log = [...this.log.slice(-499), e.payload];
        this.version++;
      }),
      await listen<DoneEvent>(EVENT_DONE, (e) => {
        if (e.payload.run_id === this.activeRunId) this.activeRunId = null;
        this.lastDone = e.payload;
        this.version++;
      }),
    );
  }

  detach() {
    for (const u of this.unlisten) u();
    this.unlisten = [];
  }

  async refreshCaps(ips: string[]) {
    const missing = ips.filter((ip) => !this.capsByIp[ip]);
    if (!missing.length) return;
    try {
      const rows = await invoke<CapsRow[]>("miner_capabilities", { ips: missing });
      for (const r of rows) this.capsByIp[r.ip] = r;
      this.version++;
    } catch (e) {
      this.error = String(e);
    }
  }

  async run(
    ips: string[],
    action: MinerAction,
    opts: { dryRun: boolean; staggerMs: number; concurrency: number; verify: boolean },
  ) {
    this.error = null;
    const req: ActionRequest = {
      ips,
      action,
      dry_run: opts.dryRun,
      stagger_ms: opts.staggerMs,
      concurrency: opts.concurrency,
      timeout_sec: 10,
      verify: opts.verify,
      verify_timeout_sec: 180,
    };
    try {
      this.activeRunId = await invoke<string>("run_miner_action", { req });
    } catch (e) {
      this.error = String(e);
    }
  }

  async cancel() {
    if (!this.activeRunId) return;
    try {
      await invoke("cancel_action_run", { runId: this.activeRunId });
    } catch (e) {
      this.error = String(e);
    }
  }
}

export const control = new ControlState();
