<script lang="ts">
  import { onMount } from "svelte";
  import { defaultDiscoverySettings, type DiscoverySettings } from "$lib/discoveryDefaults";
  import { loadDiscoverySettings, saveDiscoverySettings } from "$lib/discoverySettingsStorage";

  let settings = $state<DiscoverySettings>({ ...defaultDiscoverySettings });

  onMount(() => {
    settings = loadDiscoverySettings();
  });

  $effect(() => {
    settings.autoDiscoverIntervalSec = Math.max(5, Math.round(settings.autoDiscoverIntervalSec || 0));
    settings.autoEnrichIntervalSec = Math.max(5, Math.round(settings.autoEnrichIntervalSec || 0));
  });

  $effect(() => {
    saveDiscoverySettings(settings);
  });
</script>

<div class="mx-auto flex min-h-full w-full max-w-3xl flex-col gap-4 p-4">
  <header class="flex items-center justify-between border-b border-zinc-800 pb-3">
    <div>
      <h1 class="text-xl font-semibold tracking-tight tm-accent-text">Run settings</h1>
      <p class="text-sm text-zinc-500">Discovery probe/enrichment configuration</p>
    </div>
    <a href="/" class="rounded-lg border border-zinc-600 px-3 py-1.5 text-sm hover:bg-zinc-900">Back</a>
  </header>

  <section class="rounded-xl border border-zinc-800 bg-zinc-900/40 p-4">
    <div class="grid grid-cols-2 gap-2 text-xs">
      <label class="col-span-2 flex flex-col gap-1">
        <span class="text-zinc-500">Probe timeout (s)</span>
        <input class="rounded bg-zinc-950 px-2 py-1" type="number" bind:value={settings.probeTimeoutSec} />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-zinc-500">Probe conc.</span>
        <input class="rounded bg-zinc-950 px-2 py-1" type="number" bind:value={settings.probeConcurrency} />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-zinc-500">Enrich conc.</span>
        <input class="rounded bg-zinc-950 px-2 py-1" type="number" bind:value={settings.enrichConcurrency} />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-zinc-500">Enrich timeout (s)</span>
        <input class="rounded bg-zinc-950 px-2 py-1" type="number" bind:value={settings.enrichTimeoutSec} />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-zinc-500">Probe rate/s (0=off)</span>
        <input class="rounded bg-zinc-950 px-2 py-1" type="number" bind:value={settings.probeRateLimitPerSec} />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-zinc-500">Enrich rate/s (0=off)</span>
        <input class="rounded bg-zinc-950 px-2 py-1" type="number" bind:value={settings.enrichRateLimitPerSec} />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-zinc-500">Retry jitter (ms)</span>
        <input class="rounded bg-zinc-950 px-2 py-1" type="number" bind:value={settings.retryJitterMs} />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-zinc-500">UI coalesce (ms)</span>
        <input class="rounded bg-zinc-950 px-2 py-1" type="number" bind:value={settings.uiCoalesceMs} />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-zinc-500">Queue cap</span>
        <input class="rounded bg-zinc-950 px-2 py-1" type="number" bind:value={settings.probeQueueCap} />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-zinc-500">Enrich cap</span>
        <input class="rounded bg-zinc-950 px-2 py-1" type="number" bind:value={settings.enrichQueueCap} />
      </label>
      <label class="col-span-2 flex items-center gap-2">
        <input type="checkbox" bind:checked={settings.autoDiscoverEnabled} />
        <span class="text-zinc-500">Enable auto-discover loop</span>
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-zinc-500">Auto-discover interval (s)</span>
        <input
          class="rounded bg-zinc-950 px-2 py-1"
          type="number"
          min="5"
          bind:value={settings.autoDiscoverIntervalSec}
        />
      </label>
      <label class="col-span-2 flex items-center gap-2">
        <input type="checkbox" bind:checked={settings.autoEnrichEnabled} />
        <span class="text-zinc-500">Enable auto-enrich loop</span>
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-zinc-500">Auto-enrich interval (s)</span>
        <input
          class="rounded bg-zinc-950 px-2 py-1"
          type="number"
          min="5"
          bind:value={settings.autoEnrichIntervalSec}
        />
      </label>
    </div>
  </section>
</div>
