<script lang="ts">
  import { control } from "./controlState.svelte";
  import {
    ACTION_LABEL,
    TYPED_CONFIRM_THRESHOLD,
    unsupportedReason,
    type MinerAction,
  } from "./types";

  let {
    ips,
    action,
    onclose,
  }: { ips: string[]; action: MinerAction; onclose: () => void } = $props();

  let dryRun = $state(false);
  let verify = $state(true);
  let staggerMs = $state(1000);
  let concurrency = $state(4);
  let typed = $state("");

  $effect(() => {
    void control.refreshCaps(ips);
  });

  const rows = $derived(
    ips.map((ip) => ({ ip, reason: unsupportedReason(control.capsByIp[ip], action) })),
  );
  const supported = $derived(rows.filter((r) => !r.reason).map((r) => r.ip));
  const needsTyped = $derived(supported.length > TYPED_CONFIRM_THRESHOLD && !dryRun);
  const canConfirm = $derived(
    supported.length > 0 && (!needsTyped || typed.trim().toLowerCase() === action),
  );

  async function confirm() {
    await control.run(supported, action, { dryRun, staggerMs, concurrency, verify });
    onclose();
  }
</script>

<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60" role="presentation">
  <div
    class="w-[32rem] max-w-[92vw] rounded-xl border border-zinc-700 bg-zinc-900 p-4 text-sm text-zinc-200"
    role="dialog"
    aria-modal="true"
    aria-labelledby="tm-action-title"
  >
    <h2 id="tm-action-title" class="mb-2 text-base font-semibold">
      {ACTION_LABEL[action]} {supported.length} of {ips.length} miner{ips.length === 1 ? "" : "s"}?
    </h2>
    {#if action === "reboot"}
      <p class="mb-2 text-xs text-amber-300">Rebooting interrupts hashing for a minute or more.</p>
    {:else if action === "stop"}
      <p class="mb-2 text-xs text-amber-300">Stopped miners stop hashing until started again.</p>
    {/if}
    <ul class="mb-3 max-h-48 overflow-auto rounded border border-zinc-800 font-mono text-xs">
      {#each rows as r (r.ip)}
        <li class="flex justify-between gap-2 px-2 py-1 {r.reason ? 'text-zinc-500' : ''}">
          <span>{r.ip}</span>
          <span class="truncate text-right">{r.reason ?? (control.capsByIp[r.ip] ? "ready" : "checking…")}</span>
        </li>
      {/each}
    </ul>
    <div class="mb-3 grid grid-cols-2 gap-2 text-xs">
      <label class="flex items-center gap-2"><input type="checkbox" bind:checked={dryRun} /> Dry run (send nothing)</label>
      <label class="flex items-center gap-2"><input type="checkbox" bind:checked={verify} /> Verify after sending</label>
      <label class="flex flex-col gap-1">Stagger (ms)
        <input class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" type="number" min="0" bind:value={staggerMs} />
      </label>
      <label class="flex flex-col gap-1">Concurrency
        <input class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" type="number" min="1" max="64" bind:value={concurrency} />
      </label>
    </div>
    {#if needsTyped}
      <label class="mb-3 flex flex-col gap-1 text-xs">
        Type <span class="font-mono text-zinc-100">{action}</span> to confirm
        <input class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1 font-mono" bind:value={typed} />
      </label>
    {/if}
    <div class="flex justify-end gap-2">
      <button type="button" class="rounded-lg border border-zinc-700 px-3 py-1.5 hover:bg-zinc-800" onclick={onclose}>Cancel</button>
      <button
        type="button"
        class="rounded-lg border border-zinc-600 bg-zinc-100 px-3 py-1.5 font-medium text-zinc-900 disabled:opacity-40"
        disabled={!canConfirm}
        onclick={confirm}
      >
        {dryRun ? "Dry run" : ACTION_LABEL[action]}
      </button>
    </div>
  </div>
</div>
