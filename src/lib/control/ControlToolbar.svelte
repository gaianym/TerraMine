<script lang="ts">
  import { control } from "./controlState.svelte";
  import { ACTION_LABEL, ALL_ACTIONS, unsupportedReason, type MinerAction } from "./types";
  import { summarizeRun } from "./runSummary";

  let {
    selectedIps,
    onrequest,
  }: { selectedIps: string[]; onrequest: (ips: string[], action: MinerAction) => void } = $props();

  const actions = ALL_ACTIONS;

  function reasonFor(action: MinerAction): string | null {
    if (!selectedIps.length) return "Select one or more miners";
    const known = selectedIps.map((ip) => control.capsByIp[ip]).filter(Boolean);
    if (known.length === selectedIps.length && known.every((c) => unsupportedReason(c, action))) {
      return unsupportedReason(known[0], action);
    }
    return null;
  }
</script>

<div class="flex flex-wrap items-center gap-2 text-xs">
  <span class="text-zinc-500">{selectedIps.length} selected</span>
  {#each actions as a (a)}
    {@const reason = reasonFor(a)}
    <button
      type="button"
      class="rounded-md border border-zinc-700 px-2 py-1 hover:bg-zinc-800 disabled:cursor-not-allowed disabled:opacity-40"
      disabled={!!reason || !!control.activeRunId}
      title={reason ?? `${ACTION_LABEL[a]} selected miners`}
      onclick={() => onrequest(selectedIps, a)}
    >
      {ACTION_LABEL[a]}
    </button>
  {/each}
  {#if control.activeRunId}
    <button
      type="button"
      class="rounded-md border border-red-800 px-2 py-1 text-red-300 hover:bg-red-950"
      onclick={() => control.cancel()}
    >
      Cancel run
    </button>
  {/if}
  {#if control.error}
    <span class="text-red-400" title={control.error}>Error: {control.error}</span>
  {/if}
  {#if control.lastDone && !control.activeRunId}
    {@const s = summarizeRun(control.lastDone.results)}
    <span
      data-testid="tm-run-summary"
      data-kind={s.kind}
      class={s.kind === "dry" ? "rounded border border-dashed border-violet-700 px-1 text-violet-300" : "text-zinc-500"}
    >
      {s.text}
    </span>
  {/if}
</div>
