<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { onDestroy, onMount, untrack } from "svelte";
  import { EVENT_SAFETY_DONE, EVENT_SAFETY_ROW, type SettingsSupportRow, type WriteOutcome } from "./power";
  import { POOL_CONFIRM_TEXT, buildPoolRequest, describePools, emptyPoolInput, planPools } from "./pools";

  let {
    ips,
    onclose,
    initialSupport = null,
  }: { ips: string[]; onclose: () => void; initialSupport?: SettingsSupportRow[] | null } = $props();

  // Safety rule: new write classes open with Dry run checked.
  let dryRun = $state(true);
  let input = $state(emptyPoolInput());
  let canarySize = $state(1);
  let typed = $state("");
  let support = $state<SettingsSupportRow[] | null>(untrack(() => initialSupport));
  let error = $state<string | null>(null);
  let runId = $state<string | null>(null);
  let outcomes = $state<Record<string, WriteOutcome>>({});
  let unlisten: UnlistenFn[] = [];

  onMount(async () => {
    unlisten.push(
      await listen<WriteOutcome>(EVENT_SAFETY_ROW, (e) => {
        outcomes[e.payload.ip] = e.payload;
      }),
      await listen<{ run_id: string }>(EVENT_SAFETY_DONE, (e) => {
        if (e.payload.run_id === runId) runId = null;
      }),
    );
    if (!support) {
      try {
        support = await invoke<SettingsSupportRow[]>("settings_support", { ips });
      } catch (e) {
        error = String(e);
      }
    }
  });
  onDestroy(() => unlisten.forEach((u) => u()));

  const plan = $derived(planPools(input, support ?? [], !dryRun));
  const included = $derived(plan.rows.filter((r) => r.included).length);
  const needsTyped = $derived(!dryRun && !!plan.change.pool_op);
  const touched = $derived(input.mode !== "none" || !!input.timezone.trim());
  const canSend = $derived(
    !!support && !plan.errors.length && included > 0 && !runId && (!needsTyped || typed.trim() === POOL_CONFIRM_TEXT),
  );

  async function send() {
    const req = buildPoolRequest(plan, { dryRun, typed, canarySize });
    if (!req) return;
    error = null;
    outcomes = {};
    try {
      runId = await invoke<string>("run_settings_job", { req });
    } catch (e) {
      error = String(e);
    }
  }
  const field = "rounded border border-zinc-700 bg-zinc-950 px-2 py-1";
</script>

<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60" role="presentation">
  <div
    class="w-[44rem] max-w-[94vw] rounded-xl border border-zinc-700 bg-zinc-900 p-4 text-sm text-zinc-200"
    role="dialog"
    aria-modal="true"
    aria-labelledby="tm-pools-title"
  >
    <h2 id="tm-pools-title" class="mb-2 text-base font-semibold">Pools / timezone for {ips.length} miner{ips.length === 1 ? "" : "s"}</h2>
    <p class="mb-3 text-xs text-zinc-400">
      Sent through the safety pipeline: snapshot, read-back, a pool-alive check and automatic rollback, canary first. Add and
      replace are resolved against each miner's own pool list. Passwords are never shown or logged. A wrong pool sends
      hashrate elsewhere, so a live pool change needs typed confirmation.
    </p>
    <div class="mb-3 grid grid-cols-3 gap-2 text-xs">
      <label class="flex flex-col gap-1">Pool change
        <select data-testid="tm-pools-mode" class={field} bind:value={input.mode}>
          <option value="none">none</option>
          <option value="add">add a pool</option>
          <option value="replace">replace pool at position</option>
          <option value="set">set primary + fallback</option>
        </select>
      </label>
      {#if input.mode === "replace"}
        <label class="flex flex-col gap-1">Position (1 = primary)
          <input class={field} inputmode="numeric" bind:value={input.position} />
        </label>
      {/if}
      <label class="flex flex-col gap-1">Timezone (IANA)
        <input data-testid="tm-pools-tz" class={field} placeholder="e.g. America/Chicago" bind:value={input.timezone} />
      </label>
    </div>
    {#if input.mode !== "none"}
      <div class="mb-3 grid grid-cols-3 gap-2 text-xs">
        <input data-testid="tm-pools-url" class={field} placeholder="stratum+tcp://host:port" bind:value={input.url} />
        <input data-testid="tm-pools-user" class={field} placeholder="account.worker" bind:value={input.user} />
        <input class={field} type="password" autocomplete="off" placeholder="password (optional)" bind:value={input.password} />
        {#if input.mode === "set"}
          <input class={field} placeholder="fallback stratum+tcp://host:port" bind:value={input.url2} />
          <input class={field} placeholder="fallback account.worker" bind:value={input.user2} />
          <input class={field} type="password" autocomplete="off" placeholder="fallback password" bind:value={input.password2} />
        {/if}
      </div>
    {/if}
    <div class="mb-3 flex gap-4 text-xs">
      <label class="flex items-center gap-2"><input type="checkbox" data-testid="tm-pools-dry-run" bind:checked={dryRun} /> Dry run (send nothing)</label>
      <label class="flex items-center gap-2">Canary first wave
        <input class="w-16 {field}" type="number" min="1" bind:value={canarySize} />
      </label>
    </div>
    {#if plan.errors.length && touched}
      <ul class="mb-2 text-xs text-red-300" data-testid="tm-pools-errors">
        {#each plan.errors as e (e)}<li>{e}</li>{/each}
      </ul>
    {/if}
    <table class="mb-3 w-full table-fixed border border-zinc-800 font-mono text-xs">
      <thead class="text-zinc-500">
        <tr><th class="w-28 px-2 text-left">IP</th><th class="px-2 text-left">Model</th><th class="px-2 text-left">Current pools</th><th class="w-28 px-2 text-left">Timezone</th><th class="px-2 text-left">Plan / result</th></tr>
      </thead>
      <tbody>
        {#if !support}
          <tr><td class="px-2 py-1" colspan="5">{error ? "" : "checking…"}</td></tr>
        {:else}
          {#each support as r, i (r.ip)}
            {@const p = plan.rows[i]}
            {@const o = outcomes[r.ip]}
            <tr class={p?.included ? "" : "text-zinc-500"} data-testid="tm-pools-row">
              <td class="px-2 py-1">{r.ip}</td>
              <td class="truncate px-2 py-1" title={r.class ?? ""}>{r.model ?? "?"}</td>
              <td class="truncate px-2 py-1" title={(r.pools?.tried ?? []).join("; ")}>{r.pools?.path ? describePools(r.current_pools) : "not supported"}</td>
              <td class="truncate px-2 py-1" title={(r.timezone?.tried ?? []).join("; ")}>{r.timezone?.path ? (r.current_timezone ?? "?") : "not supported"}</td>
              <td class="truncate px-2 py-1" title={o?.msg ?? p?.note ?? ""}>{o ? `${o.status}${o.msg ? `: ${o.msg}` : ""}` : (p?.note ?? "")}</td>
            </tr>
          {/each}
        {/if}
      </tbody>
    </table>
    {#if needsTyped}
      <label class="mb-3 flex flex-col gap-1 text-xs">
        Type <span class="font-mono text-zinc-100">{POOL_CONFIRM_TEXT}</span> to send this pool change to {included} miner{included === 1 ? "" : "s"}
        <input data-testid="tm-pools-typed" class="{field} font-mono" bind:value={typed} />
      </label>
    {/if}
    {#if error}<p class="mb-2 text-xs text-red-400">{error}</p>{/if}
    <div class="flex justify-end gap-2">
      <button type="button" class="rounded-lg border border-zinc-700 px-3 py-1.5 hover:bg-zinc-800" onclick={onclose}>Close</button>
      <button
        type="button"
        class="rounded-lg border border-zinc-600 bg-zinc-100 px-3 py-1.5 font-medium text-zinc-900 disabled:opacity-40"
        disabled={!canSend}
        onclick={send}
      >
        {dryRun ? "Dry run" : `Apply to ${included}`}
      </button>
    </div>
  </div>
</div>
