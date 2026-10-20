<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { onDestroy, onMount, untrack } from "svelte";
  import {
    EVENT_DIVERT_DONE, EVENT_DIVERT_ROW, EVENT_SSH_DONE, EVENT_SSH_ROW, FALLBACK_LABELS, GRANT_CONFIRM_TEXT,
    buildDivertJob, buildSshJob, planFallback, statusTone, trustPrompt,
    type DivertOutcome, type FallbackChoice, type HostKeyInfo, type SshOutcome, type SshSupportRow,
  } from "./ssh";

  let {
    ips,
    onclose,
    initialSupport = null,
  }: { ips: string[]; onclose: () => void; initialSupport?: SshSupportRow[] | null } = $props();

  // Safety rule: new write classes open with Dry run checked.
  let dryRun = $state(true);
  let choice = $state<FallbackChoice>("restart");
  let cidr = $state("");
  let typed = $state("");
  let support = $state<SshSupportRow[] | null>(untrack(() => initialSupport));
  let error = $state<string | null>(null);
  let runId = $state<string | null>(null);
  let outcomes = $state<Record<string, SshOutcome | DivertOutcome>>({});
  // Visible trust prompt for a host key (first use or on request).
  let trust = $state<{ ip: string; fingerprint: string; pinned: string | null } | null>(null);
  // SSH login form (stored in the OS keychain; never read back).
  let loginOpen = $state(false);
  let loginUser = $state("root");
  let loginPassword = $state("");
  let loginKey = $state("");
  let loginPerDevice = $state(false);
  let loginMsg = $state<string | null>(null);
  let unlisten: UnlistenFn[] = [];

  async function refresh() {
    try {
      support = await invoke<SshSupportRow[]>("ssh_support", { ips });
    } catch (e) {
      error = String(e);
    }
  }

  onMount(async () => {
    unlisten.push(
      await listen<SshOutcome>(EVENT_SSH_ROW, (e) => {
        outcomes[e.payload.ip] = e.payload;
        const t = trustPrompt(e.payload);
        if (t && !trust) trust = { ...t, pinned: null };
      }),
      await listen<{ run_id: string }>(EVENT_SSH_DONE, (e) => {
        if (e.payload.run_id === runId) runId = null;
      }),
      await listen<DivertOutcome>(EVENT_DIVERT_ROW, (e) => {
        outcomes[e.payload.ip] = e.payload;
      }),
      await listen<{ run_id: string }>(EVENT_DIVERT_DONE, (e) => {
        if (e.payload.run_id === runId) {
          runId = null;
          refresh();
        }
      }),
    );
    if (!support) await refresh();
  });
  onDestroy(() => unlisten.forEach((u) => u()));

  const isSsh = $derived(choice === "restart" || choice === "read_logs" || choice === "grant_api_write");
  const plan = $derived(planFallback(choice, support ?? [], !dryRun, cidr));
  const included = $derived(plan.rows.filter((r) => r.included).map((r) => r.ip));
  const needsTyped = $derived(choice === "grant_api_write" && !dryRun);
  const canSend = $derived(!!support && !plan.errors.length && included.length > 0 && !runId && (!needsTyped || typed.trim() === GRANT_CONFIRM_TEXT));
  const loginClass = $derived(support?.find((r) => r.found && r.actions.some((a) => a.supported))?.class ?? null);

  async function send() {
    error = null;
    outcomes = {};
    try {
      if (isSsh) {
        const job = buildSshJob(choice as "restart" | "read_logs" | "grant_api_write", included, { dryRun, typed, cidr });
        runId = await invoke<string>("run_ssh_action", { job });
      } else {
        runId = await invoke<string>("run_pool_divert", { job: buildDivertJob(choice as "divert" | "restore", included, dryRun) });
      }
    } catch (e) {
      error = String(e);
    }
  }

  async function checkHostKey(ip: string) {
    error = null;
    try {
      const k = await invoke<HostKeyInfo>("ssh_fetch_host_key", { ip });
      trust = { ip: k.ip, fingerprint: k.fingerprint, pinned: k.pinned };
    } catch (e) {
      error = String(e);
    }
  }

  async function pin() {
    if (!trust) return;
    try {
      await invoke("ssh_pin_host_key", { ip: trust.ip, fingerprint: trust.fingerprint });
      trust = null;
      await refresh();
    } catch (e) {
      error = String(e);
    }
  }

  async function saveLogin() {
    loginMsg = null;
    if (!loginClass) return;
    try {
      await invoke("set_ssh_login", {
        input: {
          class: loginClass,
          ip: loginPerDevice && included.length === 1 ? included[0] : null,
          username: loginUser,
          password: loginKey.trim() ? null : loginPassword,
          private_key: loginKey.trim() ? loginKey : null,
        },
      });
      loginPassword = "";
      loginKey = "";
      loginMsg = "Saved to the OS keychain.";
      await refresh();
    } catch (e) {
      loginMsg = String(e);
    }
  }

  const field = "rounded border border-zinc-700 bg-zinc-950 px-2 py-1";
  const tone = { ok: "text-emerald-400", warn: "text-amber-300", bad: "text-red-400", info: "text-sky-300" } as const;
</script>

<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60" role="presentation">
  <div
    class="max-h-[92vh] w-[46rem] max-w-[94vw] overflow-y-auto rounded-xl border border-zinc-700 bg-zinc-900 p-4 text-sm text-zinc-200"
    role="dialog"
    aria-modal="true"
    aria-labelledby="tm-ssh-title"
  >
    <h2 id="tm-ssh-title" class="mb-2 text-base font-semibold">Fallbacks for {ips.length} miner{ips.length === 1 ? "" : "s"}</h2>
    <p class="mb-3 text-xs text-zinc-400">
      Used only where asic-rs or the miner's API cannot do the job. SSH sends only allow-listed commands, logs in with a login
      from the OS keychain, and needs the miner's host key pinned first. SSH paths stay dry run until confirmed on real
      hardware. Pool divert stops a miner with no native Stop by pointing it at a dead local pool; Start restores the saved pools.
    </p>

    <div class="mb-3 flex flex-wrap items-end gap-3 text-xs">
      <label class="flex flex-col gap-1">Action
        <select class={field} bind:value={choice} data-testid="tm-ssh-choice">
          {#each Object.entries(FALLBACK_LABELS) as [k, label] (k)}
            <option value={k}>{label}</option>
          {/each}
        </select>
      </label>
      {#if choice === "grant_api_write"}
        <label class="flex flex-col gap-1">LAN range given write access
          <input class={field} placeholder="a.b.c.0/24" bind:value={cidr} data-testid="tm-ssh-cidr" />
        </label>
      {/if}
      <label class="flex items-center gap-1">
        <input type="checkbox" bind:checked={dryRun} data-testid="tm-dry-run" />
        Dry run
      </label>
    </div>

    {#if choice === "grant_api_write"}
      <p class="mb-2 text-xs text-zinc-400">
        Stock S9: backs up /config/bmminer.conf, puts a W: rule first in api-allow (nothing else changes), checks the JSON,
        restarts bmminer, reads it back and checks the API now accepts write. Any failure restores the original.
      </p>
    {/if}

    <table class="mb-3 w-full text-xs">
      <thead><tr class="text-left text-zinc-400"><th class="py-1">Miner</th><th>Plan</th><th>Host key</th></tr></thead>
      <tbody>
        {#each plan.rows as r (r.ip)}
          {@const s = support?.find((x) => x.ip === r.ip)}
          <tr class="border-t border-zinc-800" data-testid="tm-ssh-row">
            <td class="py-1 pr-2 font-mono">{r.ip}</td>
            <td class={r.included ? "" : "text-zinc-500"}>{r.included ? "" : "skipped: "}{r.note}</td>
            <td>
              {#if isSsh && r.included}
                <button type="button" class="rounded border border-zinc-700 px-1 hover:bg-zinc-800" onclick={() => checkHostKey(r.ip)}>
                  {s?.access?.pinned_fingerprint ? "Pinned · check" : "Check & trust…"}
                </button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>

    {#if trust}
      <div class="mb-3 rounded border border-amber-500/60 bg-amber-950/40 p-3 text-xs" role="alertdialog" data-testid="tm-ssh-trust">
        <p class="mb-1 font-semibold">Trust this SSH host key for {trust.ip}?</p>
        <p class="mb-1 font-mono break-all">{trust.fingerprint}</p>
        {#if trust.pinned && trust.pinned !== trust.fingerprint}
          <p class="mb-1 text-red-400">This differs from the pinned key {trust.pinned}. Only re-pin if you know why it changed (e.g. a firmware reinstall).</p>
        {:else if trust.pinned}
          <p class="mb-1 text-emerald-400">Matches the pinned key.</p>
        {:else}
          <p class="mb-1 text-zinc-300">First connection. Compare it with the key on the miner before trusting it; nothing is sent until you do.</p>
        {/if}
        <div class="flex gap-2">
          {#if trust.pinned !== trust.fingerprint}
            <button type="button" class="rounded bg-amber-600 px-2 py-1 text-black hover:bg-amber-500" onclick={pin} data-testid="tm-ssh-pin">Trust and pin</button>
          {/if}
          <button type="button" class="rounded border border-zinc-700 px-2 py-1" onclick={() => (trust = null)}>Close</button>
        </div>
      </div>
    {/if}

    {#if isSsh}
      <details class="mb-3 text-xs" bind:open={loginOpen}>
        <summary class="cursor-pointer text-zinc-300">SSH login (stored in the OS keychain)</summary>
        {#if loginClass}
          <div class="mt-2 grid grid-cols-2 gap-2">
            <label class="flex flex-col gap-1">Username<input class={field} bind:value={loginUser} autocomplete="off" /></label>
            <label class="flex flex-col gap-1">Password<input class={field} type="password" bind:value={loginPassword} autocomplete="new-password" data-testid="tm-ssh-password" /></label>
            <label class="col-span-2 flex flex-col gap-1">…or private key (OpenSSH / PEM, unencrypted)
              <textarea class="{field} h-16 font-mono" bind:value={loginKey} spellcheck="false" autocomplete="off"></textarea>
            </label>
            <label class="flex items-center gap-1"><input type="checkbox" bind:checked={loginPerDevice} disabled={included.length !== 1} /> Only for this miner</label>
            <button type="button" class="justify-self-end rounded border border-zinc-700 px-2 py-1 hover:bg-zinc-800" onclick={saveLogin}>Save login</button>
          </div>
          {#if loginMsg}<p class="mt-1 text-zinc-400">{loginMsg}</p>{/if}
        {:else}
          <p class="mt-1 text-zinc-500">No selected miner has an SSH path.</p>
        {/if}
      </details>
    {/if}

    {#if needsTyped}
      <label class="mb-3 flex flex-col gap-1 text-xs">Type <code>{GRANT_CONFIRM_TEXT}</code> to confirm
        <input class={field} bind:value={typed} autocomplete="off" data-testid="tm-ssh-typed" />
      </label>
    {/if}

    {#each plan.errors as e (e)}<p class="mb-1 text-xs text-red-400">{e}</p>{/each}
    {#if error}<p class="mb-2 text-xs text-red-400">{error}</p>{/if}

    {#if Object.keys(outcomes).length}
      <div class="mb-3 space-y-2 text-xs">
        {#each Object.values(outcomes) as o (o.ip)}
          <div class="rounded border border-zinc-800 p-2">
            <div><span class="font-mono">{o.ip}</span> <span class={tone[statusTone(o.status)]}>{o.status}</span> {o.msg ?? ""}</div>
            {#if "plan" in o && o.plan.length}
              <div class="mt-1 text-zinc-400">Commands: {o.plan.join(" → ")}</div>
            {/if}
            {#if o.paths_tried.length}
              <details class="mt-1 text-zinc-400"><summary>Paths tried</summary><ul class="list-disc pl-4">{#each o.paths_tried as p, i (i)}<li>{p}</li>{/each}</ul></details>
            {/if}
            {#if "output" in o && o.output}
              <pre class="mt-1 max-h-48 overflow-auto rounded bg-zinc-950 p-2">{o.output}</pre>
            {/if}
          </div>
        {/each}
      </div>
    {/if}

    <div class="flex justify-end gap-2">
      <button type="button" class="rounded border border-zinc-700 px-3 py-1 hover:bg-zinc-800" onclick={onclose}>Close</button>
      <button
        type="button"
        class="rounded px-3 py-1 {dryRun ? 'bg-sky-700 hover:bg-sky-600' : 'bg-red-700 hover:bg-red-600'} disabled:opacity-40"
        disabled={!canSend}
        onclick={send}
        data-testid="tm-ssh-send"
      >{dryRun ? "Dry run" : "Send live"}</button>
    </div>
  </div>
</div>
