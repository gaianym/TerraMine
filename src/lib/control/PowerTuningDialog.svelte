<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { onDestroy, onMount, untrack } from "svelte";
  import { TYPED_CONFIRM_THRESHOLD } from "./types";
  import {
    EVENT_SAFETY_DONE,
    EVENT_SAFETY_ROW,
    buildSettingsRequest,
    formatRange,
    planPower,
    type SettingsSupportRow,
    type WriteOutcome,
  } from "./power";

  let {
    ips,
    onclose,
    initialSupport = null,
  }: { ips: string[]; onclose: () => void; initialSupport?: SettingsSupportRow[] | null } = $props();

  // Safety rule: new write classes open with Dry run checked.
  let dryRun = $state(true);
  let powerW = $state("");
  let tuningPct = $state("");
  let canarySize = $state(1);
  let typed = $state("");
  let support = $state<SettingsSupportRow[] | null>(untrack(() => initialSupport));
  let error = $state<string | null>(null);
  let runId = $state<string | null>(null);
  let outcomes = $state<Record<string, WriteOutcome>>({});
  let unlisten: UnlistenFn[] = [];

  onMount(async () => {
    unlisten.push(
      await listen<WriteOutcome>(EVENT_SAFETY_ROW, (e) => {
        outcomes[e.payload.ip] = e.payload;
      }),
      await listen<{ run_id: string }>(EVENT_SAFETY_DONE, (e) => {
        if (e.payload.run_id === runId) runId = null;
      }),
    );
    if (!support) {
      try {
        support = await invoke<SettingsSupportRow[]>("settings_support", { ips });
      } catch (e) {
        error = String(e);
      }
    }
  });
  onDestroy(() => unlisten.forEach((u) => u()));

  const plan = $derived(planPower({ powerW, tuningPct }, support ?? [], !dryRun));
  const included = $derived(plan.rows.filter((r) => r.included).length);
  const needsTyped = $derived(!dryRun && included > TYPED_CONFIRM_THRESHOLD);
  const canSend = $derived(
    !!support && !plan.errors.length && included > 0 && !runId && (!needsTyped || typed.trim() === "apply_settings"),
  );

  async function send() {
    const req = buildSettingsRequest(plan, { dryRun, concurrency: 4, staggerMs: 1000, canarySize, haltThreshold: 0.2 });
    if (!req) return;
    error = null;
    outcomes = {};
    try {
      runId = await invoke<string>("run_settings_job", { req });
    } catch (e) {
      error = String(e);
    }
  }
</script>

<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60" role="presentation">
  <div
    class="w-[40rem] max-w-[94vw] rounded-xl border border-zinc-700 bg-zinc-900 p-4 text-sm text-zinc-200"
    role="dialog"
    aria-modal="true"
    aria-labelledby="tm-power-title"
  >
    <h2 id="tm-power-title" class="mb-2 text-base font-semibold">Power limit / tuning for {ips.length} miner{ips.length === 1 ? "" : "s"}</h2>
    <p class="mb-3 text-xs text-zinc-400">
      Sent through the safety pipeline: limits, snapshot, read-back, health watch and automatic rollback, with a canary
      first wave. Bitaxe / AxeOS has no power control here (frequency / voltage is a separate change).
    </p>
    <div class="mb-3 grid grid-cols-2 gap-2 text-xs">
      <label class="flex flex-col gap-1">Power limit (W)
        <input data-testid="tm-power-w" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" inputmode="numeric" placeholder="e.g. 2500" bind:value={powerW} />
      </label>
      <label class="flex flex-col gap-1">Tuning percent (100 = full)
        <input data-testid="tm-tuning-pct" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" inputmode="numeric" placeholder="e.g. 80" bind:value={tuningPct} />
      </label>
      <label class="flex items-center gap-2"><input type="checkbox" data-testid="tm-power-dry-run" bind:checked={dryRun} /> Dry run (send nothing)</label>
      <label class="flex items-center gap-2">Canary first wave
        <input class="w-16 rounded border border-zinc-700 bg-zinc-950 px-2 py-1" type="number" min="1" bind:value={canarySize} />
      </label>
    </div>
    {#if plan.errors.length && (powerW || tuningPct)}
      <ul class="mb-2 text-xs text-red-300" data-testid="tm-power-errors">
        {#each plan.errors as e (e)}<li>{e}</li>{/each}
      </ul>
    {/if}
    <table class="mb-3 w-full table-fixed border border-zinc-800 font-mono text-xs">
      <thead class="text-zinc-500">
        <tr><th class="w-28 px-2 text-left">IP</th><th class="px-2 text-left">Model</th><th class="px-2 text-left">Power limit</th><th class="px-2 text-left">Tuning</th><th class="px-2 text-left">Plan / result</th></tr>
      </thead>
      <tbody>
        {#if !support}
          <tr><td class="px-2 py-1" colspan="5">{error ? "" : "checking…"}</td></tr>
        {:else}
          {#each support as r, i (r.ip)}
            {@const p = plan.rows[i]}
            {@const o = outcomes[r.ip]}
            <tr class={p?.included ? "" : "text-zinc-500"} data-testid="tm-power-row">
              <td class="px-2 py-1">{r.ip}</td>
              <td class="truncate px-2 py-1" title={r.class ?? ""}>{r.model ?? "?"}</td>
              <td class="truncate px-2 py-1" title={r.power.tried.join("; ")}>{r.power.path ? formatRange(r.power_w, "W") : "not supported"}</td>
              <td class="truncate px-2 py-1" title={r.tuning.tried.join("; ")}>{r.tuning.path ? formatRange(r.tuning_pct, "%") : "not supported"}</td>
              <td class="truncate px-2 py-1" title={o?.msg ?? p?.note ?? ""}>{o ? `${o.status}${o.msg ? `: ${o.msg}` : ""}` : (p?.note ?? "")}</td>
            </tr>
          {/each}
        {/if}
      </tbody>
    </table>
    {#if needsTyped}
      <label class="mb-3 flex flex-col gap-1 text-xs">
        Type <span class="font-mono text-zinc-100">apply_settings</span> to confirm
        <input class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1 font-mono" bind:value={typed} />
      </label>
    {/if}
    {#if error}<p class="mb-2 text-xs text-red-400">{error}</p>{/if}
    <div class="flex justify-end gap-2">
      <button type="button" class="rounded-lg border border-zinc-700 px-3 py-1.5 hover:bg-zinc-800" onclick={onclose}>Close</button>
      <button
        type="button"
        class="rounded-lg border border-zinc-600 bg-zinc-100 px-3 py-1.5 font-medium text-zinc-900 disabled:opacity-40"
        disabled={!canSend}
        onclick={send}
      >
        {dryRun ? "Dry run" : `Apply to ${included}`}
      </button>
    </div>
  </div>
</div>
