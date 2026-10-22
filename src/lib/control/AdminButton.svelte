<script lang="ts">
  import AdminDialog from "./AdminDialog.svelte";

  let { selectedIps }: { selectedIps: string[] } = $props();
  let open = $state<string[] | null>(null);
</script>

<button
  type="button"
  class="rounded-md border border-zinc-700 px-2 py-1 text-xs hover:bg-zinc-800 disabled:cursor-not-allowed disabled:opacity-40"
  disabled={!selectedIps.length}
  title={selectedIps.length ? "Hashboards, logs, password, factory reset for selected miners" : "Select one or more miners"}
  data-testid="tm-admin-open"
  onclick={() => (open = [...selectedIps])}
>
  Admin…
</button>
{#if open}
  <AdminDialog ips={open} onclose={() => (open = null)} />
{/if}
