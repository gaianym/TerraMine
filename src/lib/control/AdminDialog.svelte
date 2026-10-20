<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { onDestroy, onMount, untrack } from "svelte";
  import {
    ADMIN_ACTIONS,
    ADMIN_LABEL,
    EVENT_ADMIN_DONE,
    EVENT_ADMIN_ROW,
    type AdminAction,
    type AdminOutcome,
    type AdminSupportRow,
    boardDegraded,
    buildAdminRequest,
    confirmText,
    describeBoard,
    isDestructive,
    isRead,
    passwordError,
    planAdmin,
  } from "./admin";

  let {
    ips,
    onclose,
    initialAction = "hashboards",
    initialSupport = null,
  }: { ips: string[]; onclose: () => void; initialAction?: AdminAction; initialSupport?: AdminSupportRow[] | null } = $props();

  let action = $state<AdminAction>(untrack(() => initialAction));
  // Safety rule: destructive actions open with Dry run checked.
  let dryRun = $state(true);
  let typed = $state("");
  let acknowledged = $state(false);
  let password = $state("");
  let passwordAgain = $state("");
  let support = $state<AdminSupportRow[] | null>(untrack(() => initialSupport));
  let error = $state<string | null>(null);
  let runId = $state<string | null>(null);
  let outcomes = $state<Record<string, AdminOutcome>>({});
  let shown = $state<string | null>(null);
  let unlisten: UnlistenFn[] = [];

  onMount(async () => {
    unlisten.push(
      await listen<AdminOutcome>(EVENT_ADMIN_ROW, (e) => {
        if (e.payload.action === action) outcomes[e.payload.ip] = e.payload;
      }),
      await listen<{ run_id: string }>(EVENT_ADMIN_DONE, (e) => {
        if (e.payload.run_id === runId) runId = null;
      }),
    );
    if (!support) {
      try {
        support = await invoke<AdminSupportRow[]>("admin_support", { ips });
      } catch (e) {
        error = String(e);
      }
    }
  });
  onDestroy(() => {
    unlisten.forEach((u) => u());
    password = "";
    passwordAgain = "";
  });

  function pick(a: AdminAction) {
    action = a;
    dryRun = true;
    typed = "";
    acknowledged = false;
    password = "";
    passwordAgain = "";
    outcomes = {};
    shown = null;
  }

  const live = $derived(isDestructive(action) && !dryRun);
  const plan = $derived(planAdmin(action, support ?? [], live));
  const included = $derived(plan.rows.filter((r) => r.included).length);
  const pwError = $derived(action === "change_password" && live ? passwordError(password, passwordAgain) : null);
  const canSend = $derived(
    !!support &&
      !runId &&
      included > 0 &&
      !plan.errors.length &&
      !pwError &&
      (!live || (acknowledged && typed.trim() === confirmText(action))),
  );
  const detail = $derived(shown ? outcomes[shown] : undefined);

  async function send() {
    const req = buildAdminRequest(action, plan, { dryRun, typed, acknowledged, password, passwordAgain });
    if (!req) return;
    error = null;
    outcomes = {};
    shown = null;
    // The password leaves the form with the request; never kept here.
    password = "";
    passwordAgain = "";
    typed = "";
    acknowledged = false;
    try {
      runId = await invoke<string>("run_admin_action", { req });
    } catch (e) {
      error = String(e);
    }
  }
  const field = "rounded border border-zinc-700 bg-zinc-950 px-2 py-1";
</script>

<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60" role="presentation">
  <div
    class="flex max-h-[92vh] w-[52rem] max-w-[96vw] flex-col rounded-xl border border-zinc-700 bg-zinc-900 p-4 text-sm text-zinc-200"
    role="dialog"
    aria-modal="true"
    aria-labelledby="tm-admin-title"
  >
    <h2 id="tm-admin-title" class="mb-2 text-base font-semibold">Admin for {ips.length} miner{ips.length === 1 ? "" : "s"}</h2>
    <div class="mb-2 flex flex-wrap gap-1" role="tablist">
      {#each ADMIN_ACTIONS as a (a)}
        <button
          type="button"
          role="tab"
          aria-selected={action === a}
          data-testid="tm-admin-action-{a}"
          class="rounded-md border px-2 py-1 text-xs {action === a ? 'border-zinc-300 bg-zinc-800' : 'border-zinc-700 hover:bg-zinc-800'}"
          disabled={!!runId}
          onclick={() => pick(a)}>{ADMIN_LABEL[a]}</button
        >
      {/each}
    </div>
    <p class="mb-3 text-xs text-zinc-400" data-testid="tm-admin-note">
      {#if action === "hashboards"}
        Per-board chips, hashrate and temperatures (asic-rs <span class="font-mono">get_data</span>; bmminer stats on a stock S9). Read-only.
      {:else if action === "read_logs"}
        The miner's log (asic-rs <span class="font-mono">read_logs</span>, the S9 kernel log, or the AxeOS log stream for a few seconds). Read-only;
        log text is shown here and never written to the audit log.
      {:else if action === "change_password"}
        Changes the miner's web login. The new password goes to the OS keychain for that miner before it is sent, and is never shown or logged.
        Destructive: a lost password can lock you out.
      {:else}
        Returns the miner to factory settings: pools, tuning and login. Destructive and not undoable.
      {/if}
    </p>
    {#if isDestructive(action)}
      <div class="mb-3 flex flex-col gap-2 text-xs">
        <label class="flex items-center gap-2"
          ><input type="checkbox" data-testid="tm-admin-dry-run" bind:checked={dryRun} /> Dry run (send nothing)</label
        >
        {#if action === "change_password" && live}
          <div class="grid grid-cols-2 gap-2">
            <input
              data-testid="tm-admin-password"
              class={field}
              type="password"
              autocomplete="new-password"
              placeholder="new password"
              bind:value={password}
            />
            <input
              data-testid="tm-admin-password-again"
              class={field}
              type="password"
              autocomplete="new-password"
              placeholder="new password again"
              bind:value={passwordAgain}
            />
          </div>
          {#if pwError && (password || passwordAgain)}<p class="text-red-300">{pwError}</p>{/if}
        {/if}
      </div>
    {/if}
    {#if plan.errors.length && support}
      <ul class="mb-2 text-xs text-red-300" data-testid="tm-admin-errors">
        {#each plan.errors as e (e)}<li>{e}</li>{/each}
      </ul>
    {/if}
    <div class="mb-3 min-h-0 overflow-auto">
      <table class="w-full table-fixed border border-zinc-800 font-mono text-xs">
        <thead class="text-zinc-500">
          <tr><th class="w-28 px-2 text-left">IP</th><th class="w-40 px-2 text-left">Model / firmware</th><th class="px-2 text-left">Plan / result</th><th class="w-16"></th></tr>
        </thead>
        <tbody>
          {#if !support}
            <tr><td class="px-2 py-1" colspan="4">{error ? "" : "checking…"}</td></tr>
          {:else}
            {#each support as r, i (r.ip)}
              {@const p = plan.rows[i]}
              {@const o = outcomes[r.ip]}
              <tr class={p?.included ? "" : "text-zinc-500"} data-testid="tm-admin-row">
                <td class="px-2 py-1">{r.ip}</td>
                <td class="truncate px-2 py-1" title={`${r.class ?? ""} ${r.firmware ?? ""}`}>{r.model ?? "?"} {r.firmware ?? ""}</td>
                <td class="truncate px-2 py-1" title={o ? [o.msg ?? "", ...(o.paths_tried ?? [])].join("\n") : (p?.note ?? "")}
                  >{o ? `${o.status}${o.msg ? `: ${o.msg}` : ""}` : (p?.note ?? "")}</td
                >
                <td class="px-1 py-1 text-right">
                  {#if o?.hashboards || o?.logs != null}
                    <button type="button" class="rounded border border-zinc-700 px-1 hover:bg-zinc-800" onclick={() => (shown = shown === r.ip ? null : r.ip)}
                      >{shown === r.ip ? "hide" : "show"}</button
                    >
                  {/if}
                </td>
              </tr>
            {/each}
          {/if}
        </tbody>
      </table>
      {#if detail?.hashboards}
        <table class="mt-2 w-full border border-zinc-800 font-mono text-xs" data-testid="tm-admin-boards">
          <thead class="text-zinc-500"><tr><th class="w-16 px-2 text-left">Board</th><th class="px-2 text-left">{detail.ip} via {detail.route}</th></tr></thead>
          <tbody>
            {#each detail.hashboards as b (b.position)}
              <tr class={boardDegraded(b) ? "text-amber-300" : ""}>
                <td class="px-2 py-0.5">#{b.position}</td>
                <td class="px-2 py-0.5">{describeBoard(b)}{boardDegraded(b) ? " (degraded)" : ""}{b.serial ? ` · SN ${b.serial}` : ""}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
      {#if detail?.logs != null}
        <pre
          class="mt-2 max-h-80 overflow-auto whitespace-pre-wrap rounded border border-zinc-800 bg-zinc-950 p-2 text-[11px] text-zinc-300"
          data-testid="tm-admin-logs">{detail.logs_truncated ? "… (earlier lines cut)\n" : ""}{detail.logs}</pre>
      {/if}
    </div>
    {#if live}
      <div class="mb-3 flex flex-col gap-2 rounded border border-red-900 bg-red-950/40 p-2 text-xs" data-testid="tm-admin-confirm">
        <label class="flex items-center gap-2"
          ><input type="checkbox" data-testid="tm-admin-ack" bind:checked={acknowledged} /> I understand {ADMIN_LABEL[action].toLowerCase()} is
          destructive and cannot be undone from TerraMine</label
        >
        <label class="flex flex-col gap-1">
          <span>Type <span class="font-mono text-zinc-100">{confirmText(action)}</span> to send it to {included} miner{included === 1 ? "" : "s"}</span>
          <input data-testid="tm-admin-typed" class="{field} font-mono" autocomplete="off" bind:value={typed} />
        </label>
      </div>
    {/if}
    {#if error}<p class="mb-2 text-xs text-red-400">{error}</p>{/if}
    <div class="flex justify-end gap-2">
      <button type="button" class="rounded-lg border border-zinc-700 px-3 py-1.5 hover:bg-zinc-800" onclick={onclose}>Close</button>
      <button
        type="button"
        data-testid="tm-admin-send"
        class="rounded-lg border border-zinc-600 bg-zinc-100 px-3 py-1.5 font-medium text-zinc-900 disabled:opacity-40"
        disabled={!canSend}
        onclick={send}
      >
        {#if runId}Running…{:else if isRead(action)}{ADMIN_LABEL[action]} ({included}){:else if dryRun}Dry run{:else}{ADMIN_LABEL[action]} on {included}{/if}
      </button>
    </div>
  </div>
</div>
