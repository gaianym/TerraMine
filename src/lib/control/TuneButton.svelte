<script lang="ts">
  import TuneDialog from "./TuneDialog.svelte";

  let { selectedIps }: { selectedIps: string[] } = $props();
  let open = $state<string[] | null>(null);
</script>

<button
  type="button"
  class="rounded-md border border-zinc-700 px-2 py-1 text-xs hover:bg-zinc-800 disabled:opacity-40"
  disabled={!selectedIps.length}
  title={selectedIps.length ? "Frequency / voltage / fan / target temperature for selected miners" : "Select one or more miners"}
  data-testid="tm-tune-open"
  onclick={() => (open = [...selectedIps])}
>
  Tune…
</button>
{#if open}
  <TuneDialog ips={open} onclose={() => (open = null)} />
{/if}
