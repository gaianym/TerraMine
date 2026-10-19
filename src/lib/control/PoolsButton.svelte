<script lang="ts">
  import PoolsDialog from "./PoolsDialog.svelte";

  let { selectedIps }: { selectedIps: string[] } = $props();
  let open = $state<string[] | null>(null);
</script>

<button
  type="button"
  class="rounded-md border border-zinc-700 px-2 py-1 text-xs hover:bg-zinc-800 disabled:cursor-not-allowed disabled:opacity-40"
  disabled={!selectedIps.length}
  title={selectedIps.length ? "Pools / timezone for selected miners" : "Select one or more miners"}
  data-testid="tm-pools-open"
  onclick={() => (open = [...selectedIps])}
>
  Pools…
</button>
{#if open}
  <PoolsDialog ips={open} onclose={() => (open = null)} />
{/if}
