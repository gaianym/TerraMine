<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { onDestroy, onMount } from "svelte";
  import { EMPTY_TUNE, buildTuneChange, buildTuneRequest, restartsAxeOs, type TuneInput } from "./tune";

  type Row = { ip: string; status: string; msg: string | null };

  let { ips, onclose }: { ips: string[]; onclose: () => void } = $props();

  // Safety rule: new write classes open with Dry run checked.
  let dryRun = $state(true);
  let allowRaise = $state(false);
  let canarySize = $state(1);
  let input = $state<TuneInput>({ ...EMPTY_TUNE });
  let rows = $state<Record<string, Row>>({});
  let runId = $state<string | null>(null);
  let error = $state<string | null>(null);
  let unlisten: UnlistenFn[] = [];

  onMount(async () => {
    unlisten.push(
      await listen<Row>("safety:row", (e) => {
        if (ips.includes(e.payload.ip)) rows[e.payload.ip] = e.payload;
      }),
      await listen<{ run_id: string }>("safety:done", (e) => {
        if (e.payload.run_id === runId) runId = null;
      }),
    );
  });
  onDestroy(() => unlisten.forEach((u) => u()));

  const built = $derived(buildTuneChange(input));
  const restarts = $derived(restartsAxeOs(built.change));
  const canSend = $derived(!built.errors.length && !runId);

  async function send() {
    error = null;
    rows = {};
    try {
      runId = await invoke<string>("run_settings_job", { req: buildTuneRequest(ips, built.change, { dryRun, allowRaise, canarySize }) });
    } catch (e) {
      error = String(e);
    }
  }
</script>

<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60" role="presentation">
  <div
    class="w-[42rem] max-w-[94vw] rounded-xl border border-zinc-700 bg-zinc-900 p-4 text-sm text-zinc-200"
    role="dialog"
    aria-modal="true"
    aria-labelledby="tm-tune-title"
  >
    <h2 id="tm-tune-title" class="mb-2 text-base font-semibold">Tune {ips.length} miner{ips.length === 1 ? "" : "s"}</h2>
    <p class="mb-3 text-xs text-zinc-400">
      Frequency, core voltage, fan and target temperature through the safety pipeline: per-model limits, snapshot,
      read-back, health watch and automatic rollback, canary first wave. Leave a field empty to keep it. Run a dry run
      first to see each miner's plan.
    </p>
    <div class="mb-3 grid grid-cols-3 gap-2 text-xs">
      <label class="flex flex-col gap-1">Frequency (MHz)
        <input data-testid="tm-tune-freq" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" inputmode="numeric" bind:value={input.frequencyMhz} />
      </label>
      <label class="flex flex-col gap-1">Core voltage (mV)
        <input data-testid="tm-tune-volt" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" inputmode="numeric" bind:value={input.coreVoltageMv} />
      </label>
      <label class="flex flex-col gap-1">or power target (W)
        <input data-testid="tm-tune-power" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" inputmode="numeric" bind:value={input.powerW} />
      </label>
      <label class="flex flex-col gap-1">Fan
        <select data-testid="tm-tune-fan-mode" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" bind:value={input.fanMode}>
          <option value="keep">keep</option>
          <option value="manual">manual %</option>
          <option value="auto">auto (target temp)</option>
        </select>
      </label>
      {#if input.fanMode === "manual"}
        <label class="flex flex-col gap-1">Fan speed (%)
          <input data-testid="tm-tune-fan-pct" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" inputmode="numeric" bind:value={input.fanPct} />
        </label>
      {:else if input.fanMode === "auto"}
        <label class="flex flex-col gap-1">Target temp (°C)
          <input data-testid="tm-tune-temp" class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" inputmode="numeric" bind:value={input.tempTargetC} />
        </label>
      {/if}
    </div>
    <div class="mb-3 flex flex-wrap gap-4 text-xs">
      <label class="flex items-center gap-2"><input type="checkbox" data-testid="tm-tune-dry-run" bind:checked={dryRun} /> Dry run (send nothing)</label>
      <label class="flex items-center gap-2"><input type="checkbox" data-testid="tm-tune-allow-raise" bind:checked={allowRaise} /> Allow frequency raise</label>
      <label class="flex items-center gap-2">Canary first wave
        <input class="w-14 rounded border border-zinc-700 bg-zinc-950 px-2 py-1" type="number" min="1" bind:value={canarySize} />
      </label>
    </div>
    {#if restarts}
      <p class="mb-2 text-xs text-amber-300" data-testid="tm-tune-restart-note">
        Bitaxe (AxeOS) miners restart to apply frequency / voltage; the health watch spans the restart. A power target on
        Bitaxe becomes an estimated frequency (shown in the plan).
      </p>
    {/if}
    {#if built.errors.length && JSON.stringify(input) !== JSON.stringify(EMPTY_TUNE)}
      <ul class="mb-2 text-xs text-red-300" data-testid="tm-tune-errors">
        {#each built.errors as e (e)}<li>{e}</li>{/each}
      </ul>
    {/if}
    <table class="mb-3 w-full table-fixed border border-zinc-800 font-mono text-xs">
      <thead class="text-zinc-500"><tr><th class="w-28 px-2 text-left">IP</th><th class="w-24 px-2 text-left">Status</th><th class="px-2 text-left">Plan / result</th></tr></thead>
      <tbody>
        {#each ips as ip (ip)}
          {@const r = rows[ip]}
          <tr data-testid="tm-tune-row">
            <td class="px-2 py-1">{ip}</td>
            <td class="px-2 py-1">{r?.status ?? (runId ? "…" : "")}</td>
            <td class="truncate px-2 py-1" title={r?.msg ?? ""}>{r?.msg ?? ""}</td>
          </tr>
        {/each}
      </tbody>
    </table>
    {#if error}<p class="mb-2 text-xs text-red-400">{error}</p>{/if}
    <div class="flex justify-end gap-2">
      <button type="button" class="rounded-lg border border-zinc-700 px-3 py-1.5 hover:bg-zinc-800" onclick={onclose}>Close</button>
      <button
        type="button"
        class="rounded-lg border border-zinc-600 bg-zinc-100 px-3 py-1.5 font-medium text-zinc-900 disabled:opacity-40"
        disabled={!canSend}
        onclick={send}
      >
        {dryRun ? "Dry run" : `Apply to ${ips.length}`}
      </button>
    </div>
  </div>
</div>
