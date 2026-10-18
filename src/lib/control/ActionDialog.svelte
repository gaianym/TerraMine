<script lang="ts">
  import { control } from "./controlState.svelte";
  import { defaultActionOptions } from "./actionDefaults";
  import {
    ACTION_LABEL,
    DRY_RUN_ONLY_NOTE,
    TYPED_CONFIRM_THRESHOLD,
    dryRunOnly,
    unsupportedReason,
    type MinerAction,
  } from "./types";

  let {
    ips,
    action,
    onclose,
  }: { ips: string[]; action: MinerAction; onclose: () => void } = $props();

  // Safety rule: every dialog (per-row and bulk) opens with Dry run checked.
  const defaults = defaultActionOptions();
  let dryRun = $state(defaults.dryRun);
  let verify = $state(defaults.verify);
  let staggerMs = $state(defaults.staggerMs);
  let concurrency = $state(defaults.concurrency);
  let typed = $state("");

  $effect(() => {
    void control.refreshCaps(ips);
  });

  // Unconfirmed routes can be dry-run but are left out of a live send
  // (the backend refuses them too).
  const rows = $derived(
    ips.map((ip) => {
      const caps = control.capsByIp[ip];
      const reason = unsupportedReason(caps, action) ?? (!dryRun && dryRunOnly(caps, action) ? DRY_RUN_ONLY_NOTE : null);
      const note = !reason && dryRunOnly(caps, action) ? DRY_RUN_ONLY_NOTE : null;
      return { ip, reason, note };
    }),
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
    {:else if action === "light_on" || action === "light_off"}
      <p class="mb-2 text-xs text-zinc-400" data-testid="tm-light-note">
        Fault / locate light. Hashing is not affected. Sent through asic-rs where the firmware supports it; miners
        with no path are listed with the paths tried.
      </p>
    {/if}
    <ul class="mb-3 max-h-48 overflow-auto rounded border border-zinc-800 font-mono text-xs">
      {#each rows as r (r.ip)}
        <li class="flex justify-between gap-2 px-2 py-1 {r.reason ? 'text-zinc-500' : ''}">
          <span>{r.ip}</span>
          <span class="truncate text-right" title={r.reason ?? r.note ?? ""}>{r.reason ?? r.note ?? (control.capsByIp[r.ip] ? "ready" : "checking…")}</span>
        </li>
      {/each}
    </ul>
    <div class="mb-3 grid grid-cols-2 gap-2 text-xs">
      <label class="flex items-center gap-2"><input type="checkbox" data-testid="tm-dry-run" bind:checked={dryRun} /> Dry run (send nothing)</label>
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
