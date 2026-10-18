<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { onDestroy, onMount, untrack } from "svelte";
  import {
    EVENT_SAFETY_ALERT,
    alertTone,
    describeChange,
    describeSample,
    dismissAlert,
    pushAlert,
    retryLabel,
    type JobAlert,
  } from "./alertBanner";

  let { initial = [] }: { initial?: JobAlert[] } = $props();

  // `initial` seeds the list (tests / restored state); live alerts arrive by event.
  let alerts = $state<JobAlert[]>(untrack(() => [...initial]));
  let expanded = $state<Record<string, boolean>>({});
  let error = $state<string | null>(null);
  let unlisten: UnlistenFn | null = null;


  onMount(async () => {
    unlisten = await listen<JobAlert>(EVENT_SAFETY_ALERT, (e) => {
      alerts = pushAlert(alerts, e.payload);
    });
  });
  onDestroy(() => unlisten?.());

  async function retry(a: JobAlert) {
    error = null;
    try {
      await invoke("run_settings_job", { req: a.retry });
      alerts = dismissAlert(alerts, a.run_id);
    } catch (e) {
      error = String(e);
    }
  }
</script>

{#each alerts as a (a.run_id)}
  <div
    class="mb-2 rounded-lg border px-3 py-2 text-sm {alertTone(a) === 'danger'
      ? 'border-red-700 bg-red-950/60 text-red-100'
      : 'border-amber-700 bg-amber-950/50 text-amber-100'}"
    role="alert"
    data-testid="tm-safety-alert"
    data-tone={alertTone(a)}
  >
    <div class="flex flex-wrap items-center justify-between gap-2">
      <button
        type="button"
        class="text-left font-medium"
        aria-expanded={!!expanded[a.run_id]}
        onclick={() => (expanded[a.run_id] = !expanded[a.run_id])}
      >
        {a.headline}
      </button>
      <div class="flex gap-2">
        {#if a.retry.ips.length}
          <button type="button" class="rounded border border-current px-2 py-0.5" onclick={() => retry(a)}>
            {retryLabel(a)}
          </button>
        {/if}
        <button
          type="button"
          class="rounded border border-current px-2 py-0.5"
          onclick={() => (alerts = dismissAlert(alerts, a.run_id))}
        >
          Dismiss
        </button>
      </div>
    </div>
    {#if a.external_note}
      <p class="mt-1 opacity-90" data-testid="tm-safety-external">{a.external_note}</p>
    {/if}
    {#if expanded[a.run_id]}
      <ul class="mt-2 space-y-1">
        {#each a.groups as g (g.cause)}
          <li>
            <span class="font-medium">{g.label} ({g.count})</span>
            <ul class="ml-4 list-disc">
              {#each g.miners as m (m.ip)}
                <li>
                  <span class="font-mono">{m.ip}</span>: {describeChange(m)}{#if describeSample(m.sample)}
                    — {describeSample(m.sample)}{/if}{#if m.likely_external}
                    <span class="opacity-80"> (likely external)</span>{/if}
                </li>
              {/each}
            </ul>
          </li>
        {/each}
        {#if a.halted_ips.length}
          <li><span class="font-medium">halted ({a.halted_ips.length})</span>: {a.halted_ips.join(", ")}</li>
        {/if}
      </ul>
    {/if}
    {#if error}
      <p class="mt-1 text-red-300">{error}</p>
    {/if}
  </div>
{/each}
