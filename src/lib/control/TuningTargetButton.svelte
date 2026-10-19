<script lang="ts">
  import TuningTargetDialog from "./TuningTargetDialog.svelte";

  let { selectedIps }: { selectedIps: string[] } = $props();
  let open = $state<string[] | null>(null);
</script>

<button
  type="button"
  class="rounded-md border border-zinc-700 px-2 py-1 text-xs hover:bg-zinc-800 disabled:cursor-not-allowed disabled:opacity-40"
  disabled={!selectedIps.length}
  title={selectedIps.length ? "Tuning target, presets and scaling for selected miners" : "Select one or more miners"}
  data-testid="tm-target-open"
  onclick={() => (open = [...selectedIps])}
>
  Target…
</button>
{#if open}
  <TuningTargetDialog ips={open} onclose={() => (open = null)} />
{/if}
