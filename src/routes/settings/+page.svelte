<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import {
    cloneDiscoverySettings,
    defaultDiscoverySettings,
    finalizeDiscoverySettingsForSave,
    type DiscoverySettings,
  } from "$lib/discoveryDefaults";
  import { loadDiscoverySettings, saveDiscoverySettings } from "$lib/discoverySettingsStorage";
  import DiscoveryUIntField from "$lib/DiscoveryUIntField.svelte";

  /** Stable key order so fingerprints match across load/save and dirty checks. */
  function fingerprintFromSettings(s: DiscoverySettings): string {
    return JSON.stringify({
      probeTimeoutSec: s.probeTimeoutSec,
      probeConcurrency: s.probeConcurrency,
      probeRateLimitPerSec: s.probeRateLimitPerSec,
      enrichTimeoutSec: s.enrichTimeoutSec,
      enrichConcurrency: s.enrichConcurrency,
      enrichRateLimitPerSec: s.enrichRateLimitPerSec,
      autoDiscoverEnabled: s.autoDiscoverEnabled,
      autoDiscoverIntervalSec: s.autoDiscoverIntervalSec,
      autoEnrichEnabled: s.autoEnrichEnabled,
      autoEnrichIntervalSec: s.autoEnrichIntervalSec,
      retryJitterMs: s.retryJitterMs,
      probeQueueCap: s.probeQueueCap,
      enrichQueueCap: s.enrichQueueCap,
      uiCoalesceMs: s.uiCoalesceMs,
    });
  }

  let draft = $state<DiscoverySettings>(cloneDiscoverySettings(defaultDiscoverySettings));
  /** Last saved copy (deep-compared via JSON for dirty detection). */
  let savedFingerprint = $state("");

  /** Explicit field reads so nested `draft.*` edits invalidate dirty state reliably. */
  const draftFingerprint = $derived.by(() => fingerprintFromSettings(draft));

  const dirty = $derived(savedFingerprint !== "" && savedFingerprint !== draftFingerprint);

  let navigateAwayPending = $state(false);

  onMount(() => {
    const loaded = loadDiscoverySettings();
    draft = cloneDiscoverySettings(loaded);
    savedFingerprint = fingerprintFromSettings(draft);
  });

  function persistDraftFromUi() {
    draft = finalizeDiscoverySettingsForSave(draft);
    saveDiscoverySettings(draft);
    savedFingerprint = fingerprintFromSettings(draft);
  }

  function saveClick() {
    persistDraftFromUi();
    navigateAwayPending = false;
  }

  function revertClick() {
    if (!savedFingerprint) {
      draft = cloneDiscoverySettings(defaultDiscoverySettings);
      navigateAwayPending = false;
      return;
    }
    const parsed = JSON.parse(savedFingerprint) as DiscoverySettings;
    draft = cloneDiscoverySettings(parsed);
    navigateAwayPending = false;
  }

  function goHome() {
    void goto("/");
  }

  function backClick() {
    if (!dirty) {
      goHome();
      return;
    }
    navigateAwayPending = true;
  }

  function discardAndLeave() {
    navigateAwayPending = false;
    goHome();
  }

  function saveAndLeave() {
    persistDraftFromUi();
    navigateAwayPending = false;
    goHome();
  }

  function cancelNavigateAway() {
    navigateAwayPending = false;
  }
</script>

<div class="mx-auto flex min-h-full w-full max-w-3xl flex-col gap-4 p-4">
  <header class="flex flex-wrap items-start justify-between gap-3 border-b border-zinc-800 pb-3">
    <div>
      <h1 class="text-xl font-semibold tracking-tight tm-accent-text">Run settings</h1>
      <p class="text-sm text-zinc-500">Discovery probe/enrichment configuration</p>
      {#if dirty}
        <p class="mt-2 text-xs text-amber-400/95">You have unsaved changes. Save applies to the main discovery view.</p>
      {/if}
    </div>
    <div class="flex flex-wrap items-center gap-2">
      <button
        type="button"
        class="rounded-lg border border-zinc-600 px-3 py-1.5 text-sm hover:bg-zinc-900 disabled:cursor-not-allowed disabled:opacity-40"
        onclick={revertClick}
        disabled={!dirty}
      >
        Revert
      </button>
      <button
        type="button"
        class="rounded-lg border border-green-900/70 bg-green-900/55 px-3 py-1.5 text-sm text-green-100 hover:bg-green-900/85 disabled:cursor-not-allowed disabled:opacity-40"
        onclick={saveClick}
        disabled={!dirty}
      >
        Save
      </button>
      <button
        type="button"
        class="rounded-lg border border-zinc-600 px-3 py-1.5 text-sm hover:bg-zinc-900"
        onclick={backClick}
      >
        Back
      </button>
    </div>
  </header>

  {#if navigateAwayPending}
    <div
      class="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-amber-900/60 bg-amber-950/40 px-3 py-2 text-sm text-amber-100"
      role="status"
    >
      <span>Save settings before returning? Unsaved edits are not loaded on the main page.</span>
      <div class="flex flex-wrap gap-2">
        <button
          type="button"
          class="rounded-lg border border-zinc-600 px-2 py-1 text-xs hover:bg-zinc-900"
          onclick={cancelNavigateAway}
        >
          Cancel
        </button>
        <button
          type="button"
          class="rounded-lg border border-red-900/70 bg-red-950/55 px-2 py-1 text-xs text-red-100 hover:bg-red-950/85"
          onclick={discardAndLeave}
        >
          Discard
        </button>
        <button
          type="button"
          class="rounded-lg border border-green-900/70 bg-green-900/55 px-2 py-1 text-xs text-green-100 hover:bg-green-900/85"
          onclick={saveAndLeave}
        >
          Save &amp; Back
        </button>
      </div>
    </div>
  {/if}

  <section class="rounded-xl border border-zinc-800 bg-zinc-900/40 p-4 text-xs">
    <div class="grid grid-cols-2 gap-2">
      <DiscoveryUIntField label="Probe timeout (s)" class="col-span-2" bind:value={draft.probeTimeoutSec} min={1} />
      <DiscoveryUIntField label="Probe conc." bind:value={draft.probeConcurrency} min={1} />
      <DiscoveryUIntField label="Enrich conc." bind:value={draft.enrichConcurrency} min={1} />
      <DiscoveryUIntField label="Enrich timeout (s)" bind:value={draft.enrichTimeoutSec} min={1} />
      <DiscoveryUIntField label="Probe rate/s (0=off)" bind:value={draft.probeRateLimitPerSec} min={0} />
      <DiscoveryUIntField label="Enrich rate/s (0=off)" bind:value={draft.enrichRateLimitPerSec} min={0} />
      <DiscoveryUIntField label="Retry jitter (ms)" bind:value={draft.retryJitterMs} min={0} />
      <DiscoveryUIntField label="UI coalesce (ms)" bind:value={draft.uiCoalesceMs} min={0} />
      <DiscoveryUIntField label="Queue cap" bind:value={draft.probeQueueCap} min={1} />
      <DiscoveryUIntField label="Enrich cap" bind:value={draft.enrichQueueCap} min={1} />
      <label class="col-span-2 flex items-center gap-2">
        <input type="checkbox" bind:checked={draft.autoDiscoverEnabled} />
        <span class="text-zinc-500">Enable auto-discover loop</span>
      </label>
      <DiscoveryUIntField
        label="Auto-discover interval (s)"
        class="col-span-2"
        bind:value={draft.autoDiscoverIntervalSec}
        min={5}
      />
      <label class="col-span-2 flex items-center gap-2">
        <input type="checkbox" bind:checked={draft.autoEnrichEnabled} />
        <span class="text-zinc-500">Enable auto-enrich loop</span>
      </label>
      <DiscoveryUIntField
        label="Auto-enrich interval (s)"
        class="col-span-2"
        bind:value={draft.autoEnrichIntervalSec}
        min={5}
      />
    </div>
  </section>
</div>
