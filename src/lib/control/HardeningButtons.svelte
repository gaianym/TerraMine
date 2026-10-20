<script lang="ts">
  import AuditLogViewer from "./AuditLogViewer.svelte";
  import CapabilityMatrix from "./CapabilityMatrix.svelte";

  let { selectedIps }: { selectedIps: string[] } = $props();
  let matrixFor = $state<string[] | null>(null);
  let auditOpen = $state(false);
</script>

<button
  type="button"
  class="rounded-md border border-zinc-700 px-2 py-1 text-xs hover:bg-zinc-800"
  title={selectedIps.length ? "Capability matrix for the selected miners" : "Capability matrix for every discovered miner"}
  data-testid="tm-matrix-open"
  onclick={() => (matrixFor = [...selectedIps])}
>
  Capabilities…
</button>
<button type="button" class="rounded-md border border-zinc-700 px-2 py-1 text-xs hover:bg-zinc-800" data-testid="tm-audit-open" onclick={() => (auditOpen = true)}>
  Audit log…
</button>
{#if matrixFor}
  <CapabilityMatrix ips={matrixFor} onclose={() => (matrixFor = null)} />
{/if}
{#if auditOpen}
  <AuditLogViewer onclose={() => (auditOpen = false)} />
{/if}
