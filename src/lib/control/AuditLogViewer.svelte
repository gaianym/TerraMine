<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount, untrack } from "svelte";
  import { brief, buildAuditFilter, emptyAuditFilter, formatLocal, outcomeTone, type AuditPage } from "./audit";

  let { onclose, initialPage = null, ip = "" }: { onclose: () => void; initialPage?: AuditPage | null; ip?: string } = $props();

  const PAGE = 200;
  let filter = $state({ ...emptyAuditFilter(), ip: untrack(() => ip) });
  let page = $state<AuditPage | null>(untrack(() => initialPage));
  let loading = $state(false);
  let error = $state<string | null>(null);
  let exported = $state<string | null>(null);
  let open = $state<number | null>(null);

  async function load(more = false) {
    loading = true;
    error = null;
    try {
      const offset = more && page ? page.records.length : 0;
      const p = await invoke<AuditPage>("audit_query", { filter: buildAuditFilter(filter, PAGE, offset) });
      page = more && page ? { ...p, records: [...page.records, ...p.records] } : p;
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }
  async function exportAs(format: "csv" | "jsonl") {
    exported = null;
    try {
      const r = await invoke<{ path: string; count: number }>("audit_export", { filter: buildAuditFilter(filter, PAGE), format });
      exported = `Exported ${r.count} record${r.count === 1 ? "" : "s"} to ${r.path}`;
    } catch (e) {
      error = String(e);
    }
  }
  onMount(() => {
    if (!page) load();
  });

  const field = "rounded border border-zinc-700 bg-zinc-950 px-2 py-1";
  const tone = { ok: "text-emerald-400", warn: "text-amber-300", bad: "text-red-400", info: "text-sky-300" } as const;
</script>

<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60" role="presentation">
  <div
    class="flex max-h-[92vh] w-[96vw] max-w-[90rem] flex-col rounded-xl border border-zinc-700 bg-zinc-900 p-4 text-sm text-zinc-200"
    role="dialog"
    aria-modal="true"
    aria-labelledby="tm-audit-title"
  >
    <div class="mb-2 flex items-center gap-3">
      <h2 id="tm-audit-title" class="text-base font-semibold">Audit log</h2>
      {#if page}<span class="text-xs text-zinc-400" data-testid="tm-audit-count">{page.records.length} of {page.total}{page.skipped_lines ? ` · ${page.skipped_lines} unreadable line(s) skipped` : ""}</span>{/if}
      <button type="button" class="ml-auto rounded border border-zinc-700 px-3 py-1 hover:bg-zinc-800" onclick={onclose}>Close</button>
    </div>
    <p class="mb-2 text-xs text-zinc-400">Every write attempt and outcome, including dry runs and refusals. Secrets are redacted before anything is shown or exported.</p>

    <form class="mb-2 flex flex-wrap items-end gap-2 text-xs" onsubmit={(e) => { e.preventDefault(); load(); }}>
      <label class="flex flex-col gap-1">Miner
        <select class={field} bind:value={filter.ip} data-testid="tm-audit-ip">
          <option value="">All</option>
          {#each page?.ips ?? [] as v (v)}<option value={v}>{v}</option>{/each}
        </select>
      </label>
      <label class="flex flex-col gap-1">Action
        <select class={field} bind:value={filter.action} data-testid="tm-audit-action">
          <option value="">All</option>
          {#each page?.actions ?? [] as v (v)}<option value={v}>{v}</option>{/each}
        </select>
      </label>
      <label class="flex flex-col gap-1">Result
        <select class={field} bind:value={filter.outcome} data-testid="tm-audit-outcome">
          <option value="">All</option>
          {#each page?.outcomes ?? [] as v (v)}<option value={v}>{v}</option>{/each}
        </select>
      </label>
      <label class="flex flex-col gap-1">Phase
        <select class={field} bind:value={filter.phase}><option value="">Both</option><option value="attempt">Attempt</option><option value="outcome">Outcome</option></select>
      </label>
      <label class="flex flex-col gap-1">Mode
        <select class={field} bind:value={filter.dryRun}><option value="">Both</option><option value="dry">Dry run</option><option value="live">Live</option></select>
      </label>
      <label class="flex flex-col gap-1">From<input type="date" class={field} bind:value={filter.since} /></label>
      <label class="flex flex-col gap-1">To<input type="date" class={field} bind:value={filter.until} /></label>
      <label class="flex flex-col gap-1">Text<input class={field} bind:value={filter.text} placeholder="message, model, run id…" /></label>
      <button type="submit" class="rounded border border-zinc-700 px-2 py-1 hover:bg-zinc-800" disabled={loading}>Apply</button>
      <button type="button" class="rounded border border-zinc-700 px-2 py-1 hover:bg-zinc-800" onclick={() => { filter = emptyAuditFilter(); load(); }}>Reset</button>
      <span class="ml-auto flex gap-2">
        <button type="button" class="rounded border border-zinc-700 px-2 py-1 hover:bg-zinc-800" onclick={() => exportAs("csv")} data-testid="tm-audit-export-csv">Export CSV</button>
        <button type="button" class="rounded border border-zinc-700 px-2 py-1 hover:bg-zinc-800" onclick={() => exportAs("jsonl")}>Export JSONL</button>
      </span>
    </form>
    {#if exported}<p class="mb-2 text-xs text-emerald-300">{exported}</p>{/if}
    {#if error}<p class="mb-2 text-xs text-red-400">{error}</p>{/if}

    <div class="min-h-0 flex-1 overflow-auto">
      <table class="w-full border-collapse text-xs" data-testid="tm-audit-table">
        <thead class="sticky top-0 bg-zinc-900 text-left text-zinc-400">
          <tr><th class="py-1 pr-2">Time</th><th class="pr-2">Miner</th><th class="pr-2">Action</th><th class="pr-2">Phase</th><th class="pr-2">Result</th><th class="pr-2">Mode</th><th>Message</th></tr>
        </thead>
        <tbody>
          {#each page?.records ?? [] as r, i (i)}
            <tr class="cursor-pointer border-t border-zinc-800 hover:bg-zinc-800/50" onclick={() => (open = open === i ? null : i)} data-testid="tm-audit-row">
              <td class="py-1 pr-2 whitespace-nowrap">{formatLocal(r.ts_ms)}</td>
              <td class="pr-2 font-mono">{r.ip}</td>
              <td class="pr-2">{r.action}{r.irreversible ? " ⚠" : ""}</td>
              <td class="pr-2">{r.phase}</td>
              <td class="pr-2 {tone[outcomeTone(r.outcome)]}">{r.outcome}</td>
              <td class="pr-2">{r.dry_run ? "dry run" : "live"}</td>
              <td class="max-w-[40rem] truncate">{r.msg ?? ""}</td>
            </tr>
            {#if open === i}
              <tr><td colspan="7" class="bg-zinc-950 p-2 text-zinc-300">
                <div>Run {r.run_id} · {r.device_class ?? "?"} · {r.model ?? "?"} · {r.firmware ?? "?"}{r.snapshot_id ? ` · snapshot ${r.snapshot_id}` : ""}</div>
                {#if r.msg}<div class="mt-1 whitespace-pre-wrap">{r.msg}</div>{/if}
                {#if r.requested}<div class="mt-1 font-mono text-[11px]">requested: {brief(r.requested)}</div>{/if}
                {#if r.pre_state}<div class="font-mono text-[11px]">before: {brief(r.pre_state)}</div>{/if}
                {#if r.paths_tried.length}<ol class="mt-1 list-decimal pl-5">{#each r.paths_tried as p, j (j)}<li>{p}</li>{/each}</ol>{/if}
              </td></tr>
            {/if}
          {/each}
        </tbody>
      </table>
      {#if page && page.records.length < page.total}
        <button type="button" class="mt-2 rounded border border-zinc-700 px-2 py-1 text-xs hover:bg-zinc-800" onclick={() => load(true)} disabled={loading}>Load more</button>
      {/if}
      {#if page && !page.total}<p class="mt-2 text-xs text-zinc-500">No records match.</p>{/if}
    </div>
  </div>
</div>
