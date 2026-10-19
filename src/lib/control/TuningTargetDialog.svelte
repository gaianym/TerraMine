<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { onDestroy, onMount, untrack } from "svelte";
  import { TYPED_CONFIRM_THRESHOLD } from "./types";
  import {
    EVENT_SAFETY_DONE,
    EVENT_SAFETY_ROW,
    buildSettingsRequest,
    type SettingsSupportRow,
    type WriteOutcome,
  } from "./power";
  import { MODE_LABEL, commonPresets, planTarget, type TargetKind } from "./tuningTarget";
  import type { TuningMode } from "./power";

  let {
    ips,
    onclose,
    initialSupport = null,
  }: { ips: string[]; onclose: () => void; initialSupport?: SettingsSupportRow[] | null } = $props();

  // Safety rule: new write classes open with Dry run checked.
  let dryRun = $state(true);
  let kind = $state<TargetKind>("none");
  let mode = $state<TuningMode | "">("");
  let hashrateThs = $state("");
  let preset = $state("");
  let scalingStep = $state("");
  let scalingMin = $state("");
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

  const plan = $derived(planTarget({ kind, mode, hashrateThs, preset, scalingStep, scalingMin }, support ?? [], !dryRun));
  const presets = $derived(commonPresets(support ?? []));
  const touched = $derived(kind !== "none" || !!scalingStep || !!scalingMin);
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
    class="w-[44rem] max-w-[94vw] rounded-xl border border-zinc-700 bg-zinc-900 p-4 text-sm text-zinc-200"
    role="dialog"
    aria-modal="true"
    aria-labelledby="tm-target-title"
  >
    <h2 id="tm-target-title" class="mb-2 text-base font-semibold">Tuning target for {ips.length} miner{ips.length === 1 ? "" : "s"}</h2>
    <p class="mb-3 text-xs text-zinc-400">
      One target per miner: a mining mode, a hashrate target or a firmware preset (power targets are under Power…), plus
      optional scaling. Sent through the safety pipeline with a canary first wave. Bitaxe / AxeOS has no modes or hashrate
      target: they become an estimated frequency, shown in the dry-run plan. High mode is not allowed.
    </p>
    <div class="mb-3 grid grid-cols-2 gap-2 text-xs">
      <label class="flex flex-col gap-1">Target
        <select data-testid="tm-target-kind" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" bind:value={kind}>
          <option value="none">No target change</option>
          <option value="mode">Mining mode</option>
          <option value="hashrate">Hashrate target</option>
          <option value="preset">Preset</option>
        </select>
      </label>
      {#if kind === "mode"}
        <label class="flex flex-col gap-1">Mode
          <select data-testid="tm-target-mode" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" bind:value={mode}>
            <option value="">Pick…</option>
            {#each ["low", "normal"] as const as m (m)}<option value={m}>{MODE_LABEL[m]}</option>{/each}
          </select>
        </label>
      {:else if kind === "hashrate"}
        <label class="flex flex-col gap-1">Hashrate (TH/s)
          <input data-testid="tm-target-ths" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" inputmode="decimal" placeholder="e.g. 90 or 1.05" bind:value={hashrateThs} />
        </label>
      {:else if kind === "preset"}
        <label class="flex flex-col gap-1">Preset
          <select data-testid="tm-target-preset" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" bind:value={preset}>
            <option value="">{presets.length ? "Pick…" : "no preset common to the selection"}</option>
            {#each presets as p (p)}<option value={p}>{p}</option>{/each}
          </select>
        </label>
      {:else}
        <span></span>
      {/if}
      <label class="flex flex-col gap-1">Scaling step
        <input data-testid="tm-scaling-step" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" inputmode="numeric" placeholder="optional" bind:value={scalingStep} />
      </label>
      <label class="flex flex-col gap-1">Scaling minimum
        <input data-testid="tm-scaling-min" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" inputmode="numeric" placeholder="optional" bind:value={scalingMin} />
      </label>
      <label class="flex items-center gap-2"><input type="checkbox" data-testid="tm-target-dry-run" bind:checked={dryRun} /> Dry run (send nothing)</label>
      <label class="flex items-center gap-2">Canary first wave
        <input class="w-16 rounded border border-zinc-700 bg-zinc-950 px-2 py-1" type="number" min="1" bind:value={canarySize} />
      </label>
    </div>
    {#if plan.errors.length && touched}
      <ul class="mb-2 text-xs text-red-300" data-testid="tm-target-errors">
        {#each plan.errors as e (e)}<li>{e}</li>{/each}
      </ul>
    {/if}
    <table class="mb-3 w-full table-fixed border border-zinc-800 font-mono text-xs">
      <thead class="text-zinc-500">
        <tr><th class="w-28 px-2 text-left">IP</th><th class="px-2 text-left">Model</th><th class="px-2 text-left">Modes</th><th class="px-2 text-left">Presets</th><th class="px-2 text-left">Plan / result</th></tr>
      </thead>
      <tbody>
        {#if !support}
          <tr><td class="px-2 py-1" colspan="5">{error ? "" : "checking…"}</td></tr>
        {:else}
          {#each support as r, i (r.ip)}
            {@const p = plan.rows[i]}
            {@const o = outcomes[r.ip]}
            <tr class={p?.included ? "" : "text-zinc-500"} data-testid="tm-target-row">
              <td class="px-2 py-1">{r.ip}</td>
              <td class="truncate px-2 py-1" title={r.class ?? ""}>{r.model ?? "?"}</td>
              <td class="truncate px-2 py-1" title={(r.mode?.tried ?? []).join("; ")}>{r.mode?.path ? (r.tuning_modes ?? []).map((m) => MODE_LABEL[m]).join(", ") || "none allowed" : "not supported"}</td>
              <td class="truncate px-2 py-1" title={(r.preset?.tried ?? []).join("; ")}>{r.preset?.path ? (r.presets ?? []).join(", ") || "none listed" : "not supported"}</td>
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
        data-testid="tm-target-send"
        class="rounded-lg border border-zinc-600 bg-zinc-100 px-3 py-1.5 font-medium text-zinc-900 disabled:opacity-40"
        disabled={!canSend}
        onclick={send}
      >
        {dryRun ? "Dry run" : `Apply to ${included}`}
      </button>
    </div>
  </div>
</div>
