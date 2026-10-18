<script lang="ts">
  import PowerTuningDialog from "./PowerTuningDialog.svelte";

  let { selectedIps }: { selectedIps: string[] } = $props();
  let open = $state<string[] | null>(null);
</script>

<button
  type="button"
  class="rounded-md border border-zinc-700 px-2 py-1 text-xs hover:bg-zinc-800 disabled:cursor-not-allowed disabled:opacity-40"
  disabled={!selectedIps.length}
  title={selectedIps.length ? "Power limit / tuning percent for selected miners" : "Select one or more miners"}
  data-testid="tm-power-open"
  onclick={() => (open = [...selectedIps])}
>
  Power…
</button>
{#if open}
  <PowerTuningDialog ips={open} onclose={() => (open = null)} />
{/if}
