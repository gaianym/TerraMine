<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount, untrack } from "svelte";
  import {
    STATUS_TEXT, cellClass, cellGlyph, columns, groupByFirmware, summarize,
    type MatrixCell, type MatrixRow,
  } from "./matrix";

  let {
    ips = [],
    onclose,
    initialRows = null,
  }: { ips?: string[]; onclose: () => void; initialRows?: MatrixRow[] | null } = $props();

  let rows = $state<MatrixRow[] | null>(untrack(() => initialRows));
  let view = $state<"miner" | "firmware">("miner");
  let group = $state<"all" | "control" | "settings">("all");
  let loading = $state(false);
  let error = $state<string | null>(null);
  /** Paths-tried drawer. */
  let drawer = $state<{ title: string; members: Array<{ ip: string; cell: MatrixCell }> } | null>(null);
  let nativeFor = $state<string | null>(null);

  async function load() {
    loading = true;
    error = null;
    try {
      rows = await invoke<MatrixRow[]>("capability_matrix", { ips: ips.length ? ips : null });
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }
  onMount(() => {
    if (!rows) load();
  });

  const cols = $derived(columns(rows ?? []).filter((c) => group === "all" || c.group === group));
  const groups = $derived(groupByFirmware(rows ?? []));
  const totals = $derived(summarize(rows ?? []));
  const cellOf = (r: MatrixRow, action: string) => r.cells.find((c) => c.action === action);
</script>

<div class="fixed inset-0 z-50 flex items-center justify-center bg-black/60" role="presentation">
  <div
    class="flex max-h-[92vh] w-[96vw] max-w-[110rem] flex-col rounded-xl border border-zinc-700 bg-zinc-900 p-4 text-sm text-zinc-200"
    role="dialog"
    aria-modal="true"
    aria-labelledby="tm-matrix-title"
  >
    <div class="mb-2 flex flex-wrap items-center gap-3">
      <h2 id="tm-matrix-title" class="text-base font-semibold">Capability matrix — {ips.length ? `${ips.length} selected` : "all discovered miners"}</h2>
      <div class="flex overflow-hidden rounded border border-zinc-700 text-xs" role="tablist">
        <button type="button" class="px-2 py-1 {view === 'miner' ? 'bg-zinc-700' : ''}" onclick={() => (view = "miner")} data-testid="tm-matrix-view-miner">Per miner</button>
        <button type="button" class="px-2 py-1 {view === 'firmware' ? 'bg-zinc-700' : ''}" onclick={() => (view = "firmware")} data-testid="tm-matrix-view-firmware">Per firmware</button>
      </div>
      <select class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1 text-xs" bind:value={group}>
        <option value="all">All actions</option>
        <option value="control">Control</option>
        <option value="settings">Settings</option>
      </select>
      <button type="button" class="rounded border border-zinc-700 px-2 py-1 text-xs hover:bg-zinc-800 disabled:opacity-40" onclick={load} disabled={loading}>{loading ? "Reading…" : "Refresh"}</button>
      {#if rows}
        <span class="text-xs text-zinc-400" data-testid="tm-matrix-totals">
          {totals.miners} miner{totals.miners === 1 ? "" : "s"}{totals.missing ? `, ${totals.missing} not found` : ""} ·
          <span class="text-emerald-300">{totals.confirmed} live</span> ·
          <span class="text-amber-200">{totals.dry_run_only} dry run only</span> ·
          {totals.unsupported} not supported
        </span>
      {/if}
      <button type="button" class="ml-auto rounded border border-zinc-700 px-3 py-1 hover:bg-zinc-800" onclick={onclose}>Close</button>
    </div>
    <p class="mb-2 text-xs text-zinc-400">
      Read live from TerraMine's capability gates and the asic-rs supports_* flags; nothing is written. "Live" = the path is
      confirmed on real hardware; "dry run only" = supported, but live sends are refused until it is hardware-confirmed. Click a
      cell for every path tried.
    </p>
    {#if error}<p class="mb-2 text-xs text-red-400">{error}</p>{/if}

    <div class="min-h-0 flex-1 overflow-auto">
      {#if rows && view === "miner"}
        <table class="w-full border-collapse text-xs" data-testid="tm-matrix-table">
          <thead class="sticky top-0 bg-zinc-900">
            <tr class="text-left text-zinc-400">
              <th class="py-1 pr-2">Miner</th>
              <th class="pr-2">Firmware</th>
              {#each cols as c (c.action)}<th class="px-1 font-normal whitespace-nowrap">{c.label}</th>{/each}
            </tr>
          </thead>
          <tbody>
            {#each rows as r (r.ip)}
              <tr class="border-t border-zinc-800" data-testid="tm-matrix-row">
                <td class="py-1 pr-2 font-mono whitespace-nowrap">
                  {r.ip}
                  {#if r.native.length}
                    <button type="button" class="ml-1 text-[10px] text-sky-300 underline" onclick={() => (nativeFor = nativeFor === r.ip ? null : r.ip)}>asic-rs flags</button>
                  {/if}
                </td>
                <td class="pr-2 whitespace-nowrap text-zinc-300">
                  {#if r.found}{r.model} · {r.firmware} <span class="text-zinc-500">({r.driver})</span>{:else}<span class="text-red-300">not found{r.error ? `: ${r.error}` : ""}</span>{/if}
                </td>
                {#each cols as c (c.action)}
                  {@const cell = cellOf(r, c.action)}
                  <td class="px-1">
                    {#if cell}
                      <button
                        type="button"
                        class="w-full rounded px-1 py-0.5 text-left whitespace-nowrap {cellClass(cell.status)}"
                        title={`${STATUS_TEXT[cell.status]}${cell.path ? ` via ${cell.path}` : ""}`}
                        onclick={() => (drawer = { title: `${c.label} on ${r.ip}`, members: [{ ip: r.ip, cell }] })}
                      >{cellGlyph(cell.status)}{cell.path ? ` · ${cell.path}` : ""}</button>
                    {/if}
                  </td>
                {/each}
              </tr>
              {#if nativeFor === r.ip}
                <tr><td colspan={cols.length + 2} class="pb-2 text-[11px] text-zinc-400">
                  {#each r.native as f (f.name)}<span class="mr-2 {f.supported ? 'text-emerald-300' : 'text-zinc-500'}">{f.name}={f.supported}</span>{/each}
                </td></tr>
              {/if}
            {/each}
          </tbody>
        </table>
      {:else if rows}
        <table class="w-full border-collapse text-xs" data-testid="tm-matrix-firmware-table">
          <thead class="sticky top-0 bg-zinc-900">
            <tr class="text-left text-zinc-400">
              <th class="py-1 pr-2">Firmware (class · model · firmware)</th>
              <th class="pr-2">Miners</th>
              {#each cols as c (c.action)}<th class="px-1 font-normal whitespace-nowrap">{c.label}</th>{/each}
            </tr>
          </thead>
          <tbody>
            {#each groups as g (g.key)}
              <tr class="border-t border-zinc-800">
                <td class="py-1 pr-2 whitespace-nowrap">{g.class} · {g.model} · {g.firmware}</td>
                <td class="pr-2">{g.ips.length}</td>
                {#each cols as c (c.action)}
                  {@const gc = g.cells[c.action]}
                  <td class="px-1">
                    {#if gc}
                      <button
                        type="button"
                        class="w-full rounded px-1 py-0.5 text-left whitespace-nowrap {cellClass(gc.status)}"
                        title={gc.status === "mixed" ? `${gc.counts.confirmed} live, ${gc.counts.dry_run_only} dry run, ${gc.counts.unsupported} none` : STATUS_TEXT[gc.status]}
                        onclick={() => (drawer = { title: `${c.label} on ${g.model} · ${g.firmware}`, members: gc.members })}
                      >{cellGlyph(gc.status)}{gc.paths.length ? ` · ${gc.paths.join(", ")}` : ""}</button>
                    {/if}
                  </td>
                {/each}
              </tr>
            {/each}
          </tbody>
        </table>
      {:else if loading}
        <p class="text-xs text-zinc-400">Reading capabilities…</p>
      {/if}
    </div>

    {#if drawer}
      <aside class="mt-3 max-h-64 overflow-auto rounded border border-zinc-700 bg-zinc-950 p-3 text-xs" data-testid="tm-paths-drawer">
        <div class="mb-1 flex items-center">
          <span class="font-semibold">Paths tried — {drawer.title}</span>
          <button type="button" class="ml-auto rounded border border-zinc-700 px-2" onclick={() => (drawer = null)}>Close</button>
        </div>
        {#each drawer.members as m (m.ip)}
          <div class="mb-2">
            <div><span class="font-mono">{m.ip}</span> — <span class={cellClass(m.cell.status)}>{STATUS_TEXT[m.cell.status]}</span>{m.cell.path ? ` via ${m.cell.path}` : ""}</div>
            <ol class="list-decimal pl-5 text-zinc-300">{#each m.cell.paths_tried as p, i (i)}<li>{p}</li>{/each}</ol>
          </div>
        {/each}
      </aside>
    {/if}
  </div>
</div>
