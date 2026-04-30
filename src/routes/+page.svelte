<script lang="ts">
  import {
    ClientSideRowModelModule,
    ModuleRegistry,
    createGrid,
    type ColDef,
    type GridApi,
    type GridOptions,
    type ValueFormatterParams,
  } from "ag-grid-community";
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { onDestroy, onMount } from "svelte";
  import { Settings } from "lucide-svelte";
  import {
    defaultDiscoverySettings,
    type DiscoveryProgress,
    type DiscoveryRowEvent,
    type DiscoverySettings,
  } from "$lib/discoveryDefaults";
  import {
    loadDiscoverySettings,
    loadTargetProfiles,
    saveTargetProfiles,
    type TargetProfile,
  } from "$lib/discoverySettingsStorage";

  let targets = $state("192.168.1.0/24");
  let settings = $state<DiscoverySettings>({ ...defaultDiscoverySettings });
  let runId = $state<string | null>(null);
  let busy = $state(false);
  let errorMsg = $state<string | null>(null);

  const emptyProgress: DiscoveryProgress = {
    totalTargets: 0,
    probed: 0,
    alive: 0,
    missed: 0,
    enriched: 0,
    partial: 0,
    cancelled: 0,
  };
  let progress = $state<DiscoveryProgress>({ ...emptyProgress });
  let rows = $state<Map<string, DiscoveryRowEvent>>(new Map());
  let targetProfiles = $state<TargetProfile[]>([]);
  let selectedProfileId = $state("");

  let unlistenFns: UnlistenFn[] = [];
  let coalesceTimer: ReturnType<typeof setTimeout> | null = null;
  let pendingRows = new Map<string, DiscoveryRowEvent>();
  let removeWindowErrorListeners: (() => void) | null = null;
  let gridHost: HTMLDivElement | null = null;
  let gridApi: GridApi<GridRow> | null = null;

  type GridRow = {
    id: string;
    status: DiscoveryRowEvent["status"];
    [key: string]: unknown;
  };
  const GRID_COLUMN_KEYS_STORAGE_KEY = "tm:discovery:grid-column-keys:v1";
  const BASELINE_COLUMN_KEYS = ["ip", "discovery"];

  function isFinalStatus(status: DiscoveryRowEvent["status"]) {
    return status === "Enriched" || status === "Partial";
  }

  function loadGridColumnKeys(): string[] {
    if (typeof window === "undefined") return [...BASELINE_COLUMN_KEYS];
    try {
      const raw = window.localStorage.getItem(GRID_COLUMN_KEYS_STORAGE_KEY);
      if (!raw) return [...BASELINE_COLUMN_KEYS];
      const parsed = JSON.parse(raw);
      if (!Array.isArray(parsed)) return [...BASELINE_COLUMN_KEYS];
      const keys = parsed.filter((key): key is string => typeof key === "string" && key.length > 0);
      return [...new Set([...BASELINE_COLUMN_KEYS, ...keys])];
    } catch {
      return [...BASELINE_COLUMN_KEYS];
    }
  }

  function saveGridColumnKeys(keys: string[]) {
    if (typeof window === "undefined") return;
    try {
      window.localStorage.setItem(GRID_COLUMN_KEYS_STORAGE_KEY, JSON.stringify(keys));
    } catch {
      // ignore storage write failures
    }
  }

  ModuleRegistry.registerModules([ClientSideRowModelModule]);

  function formatGridValue(value: unknown) {
    if (value === undefined) return "\u2014";
    if (typeof value === "string") return value;
    return JSON.stringify(value);
  }

  function formatHashRateValue(value: unknown) {
    if (value === undefined || value === null) return "\u2014";
    const unitTiers = ["H/s", "KH/s", "MH/s", "GH/s", "TH/s", "PH/s", "EH/s", "ZH/s"] as const;
    const normalizeHashRate = (amount: number, unit: string) => {
      let idx = unitTiers.indexOf(unit as (typeof unitTiers)[number]);
      if (idx < 0) return { amount, unit };
      if (amount <= 0) {
        return { amount: 0, unit: "H/s" as const };
      }
      while (amount >= 1000 && idx < unitTiers.length - 1) {
        amount /= 1000;
        idx += 1;
      }
      while (amount < 1 && idx > 0) {
        amount *= 1000;
        idx -= 1;
      }
      return { amount, unit: unitTiers[idx] };
    };
    const unitMap: Record<string, string> = {
      hash: "H/s",
      h: "H/s",
      hs: "H/s",
      "h/s": "H/s",
      kilohash: "KH/s",
      khash: "KH/s",
      kh: "KH/s",
      khs: "KH/s",
      "kh/s": "KH/s",
      megahash: "MH/s",
      mhash: "MH/s",
      mh: "MH/s",
      mhs: "MH/s",
      "mh/s": "MH/s",
      gigahash: "GH/s",
      ghash: "GH/s",
      gh: "GH/s",
      ghs: "GH/s",
      "gh/s": "GH/s",
      terahash: "TH/s",
      thash: "TH/s",
      th: "TH/s",
      ths: "TH/s",
      "th/s": "TH/s",
      petahash: "PH/s",
      phash: "PH/s",
      ph: "PH/s",
      phs: "PH/s",
      "ph/s": "PH/s",
      exahash: "EH/s",
      ehash: "EH/s",
      eh: "EH/s",
      ehs: "EH/s",
      "eh/s": "EH/s",
      zettahash: "ZH/s",
      zhash: "ZH/s",
      zh: "ZH/s",
      zhs: "ZH/s",
      "zh/s": "ZH/s",
    };
    const formatAmount = (amount: number, unit?: string) => {
      if (!Number.isFinite(amount)) return "\u2014";
      if (unit) {
        const scaled = normalizeHashRate(amount, unit);
        return `${scaled.amount.toFixed(2)} ${scaled.unit}`;
      }
      return `${Math.max(0, amount).toFixed(2)} H/s`;
    };
    if (typeof value === "object") {
      const record = value as Record<string, unknown>;
      const rawAmount = record.value;
      const rawUnit = record.unit;
      const amount =
        typeof rawAmount === "number"
          ? rawAmount
          : typeof rawAmount === "string"
            ? Number.parseFloat(rawAmount)
            : Number.NaN;
      const formattedAmount = formatAmount(amount);
      if (formattedAmount !== null) {
        const unitKey =
          typeof rawUnit === "string" ? rawUnit.trim().toLowerCase().replace(/\s+/g, "") : "";
        const unit = unitMap[unitKey] ?? (typeof rawUnit === "string" ? rawUnit.trim() : "");
        return unit ? formatAmount(amount, unit) : formattedAmount;
      }
    }
    if (typeof value === "number" && Number.isFinite(value)) {
      return value.toFixed(2);
    }
    if (typeof value === "string") {
      const trimmed = value.trim();
      const match = trimmed.match(/^(-?\d+(?:\.\d+)?)(.*)$/);
      if (!match) return trimmed;
      const amount = Number.parseFloat(match[1]);
      if (!Number.isFinite(amount)) return trimmed;
      const rawUnit = match[2].trim();
      const unitKey = rawUnit.toLowerCase().replace(/\s+/g, "");
      const unit = unitMap[unitKey] ?? rawUnit;
      return formatAmount(amount, unit);
    }
    return formatGridValue(value);
  }

  function formatColumnHeader(col: string) {
    const explicit: Record<string, string> = {
      ip: "IP",
      mac: "MAC",
      hashrate: "HashRate",
      average_temperature: "Avg Temp",
    };
    const mapped = explicit[col];
    if (mapped) return mapped;
    return col
      .split("_")
      .filter(Boolean)
      .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
      .join(" ");
  }

  function refreshGridLayout() {
    if (!gridApi) return;
    gridApi.refreshCells({ force: true });
    gridApi.autoSizeAllColumns(false);
    // Keep each column's minWidth aligned to its latest autosized width.
    for (const column of gridApi.getAllDisplayedColumns()) {
      const width = column.getActualWidth();
      column.getColDef().minWidth = width;
    }
  }

  function applyTargetProfile(profileId: string) {
    selectedProfileId = profileId;
    const profile = targetProfiles.find((p) => p.id === profileId);
    if (profile) {
      targets = profile.targets;
    }
  }

  function saveTargetProfile() {
    const value = targets.trim();
    if (!value) return;
    const existing = targetProfiles.find((p) => p.id === selectedProfileId);
    if (existing) {
      existing.targets = value;
      targetProfiles = [...targetProfiles];
      return;
    }

    const name = window.prompt("Profile name", `Targets ${targetProfiles.length + 1}`)?.trim();
    if (!name) return;
    const id = typeof crypto !== "undefined" && "randomUUID" in crypto ? crypto.randomUUID() : `${Date.now()}`;
    targetProfiles = [...targetProfiles, { id, name, targets: value }];
    selectedProfileId = id;
  }

  function deleteTargetProfile() {
    if (!selectedProfileId) return;
    targetProfiles = targetProfiles.filter((p) => p.id !== selectedProfileId);
    selectedProfileId = "";
  }

  function flushRows() {
    if (pendingRows.size === 0) return;
    const next = new Map(rows);
    for (const [k, v] of pendingRows) {
      const existing = next.get(k);
      if (existing && isFinalStatus(existing.status) && !isFinalStatus(v.status)) continue;
      next.set(k, v);
    }
    pendingRows.clear();
    rows = next;
  }

  function scheduleCoalesce() {
    const ms = Math.max(0, settings.uiCoalesceMs);
    if (ms === 0) {
      flushRows();
      return;
    }
    if (coalesceTimer) clearTimeout(coalesceTimer);
    coalesceTimer = setTimeout(() => {
      coalesceTimer = null;
      flushRows();
    }, ms);
  }

  function applyRow(ev: DiscoveryRowEvent) {
    if (isFinalStatus(ev.status)) {
      // Show finalized discovery rows immediately when enrichment completes.
      pendingRows.delete(ev.id);
      rows = new Map(rows).set(ev.id, ev);
      return;
    }
    const existing = rows.get(ev.id);
    if (existing && isFinalStatus(existing.status)) return;
    pendingRows.set(ev.id, ev);
    scheduleCoalesce();
  }

  async function wireEvents(id: string) {
    for (const u of unlistenFns) {
      void u();
    }
    unlistenFns = [];

    const rowEv = `tm:discovery:row:${id}`;
    const progEv = `tm:discovery:progress:${id}`;
    const doneEv = `tm:discovery:done:${id}`;
    const errEv = `tm:discovery:error:${id}`;

    try {
      unlistenFns.push(
        await listen<DiscoveryRowEvent>(rowEv, (e) => {
          applyRow(e.payload);
        }),
      );
      unlistenFns.push(
        await listen<DiscoveryProgress>(progEv, (e) => {
          progress = e.payload;
        }),
      );
      unlistenFns.push(
        await listen(doneEv, () => {
          busy = false;
          runId = null;
          flushRows();
        }),
      );
      unlistenFns.push(
        await listen<{ code: string; message: string }>(errEv, (e) => {
          errorMsg = `${e.payload.code}: ${e.payload.message}`;
          busy = false;
        }),
      );
    } catch (e) {
      throw e;
    }
  }

  async function startRun() {
    errorMsg = null;
    rows = new Map();
    progress = { ...emptyProgress };
    pendingRows.clear();
    busy = true;
    const id =
      typeof crypto !== "undefined" && "randomUUID" in crypto
        ? crypto.randomUUID()
        : `${Date.now()}-${Math.random().toString(16).slice(2)}`;
    try {
      await wireEvents(id);
      await invoke<string>("start_discovery_run", {
        runId: id,
        targets: targets.trim(),
        settings,
      });
      runId = id;
    } catch (e) {
      busy = false;
      errorMsg = String(e);
    }
  }

  async function stopRun() {
    if (!runId) {
      try {
        await invoke<number>("stop_all_discovery_runs");
        busy = false;
        return;
      } catch (e) {
        throw e;
      }
    }
    try {
      await invoke("stop_discovery_run", { runId: runId });
    } catch {
      await invoke<number>("stop_all_discovery_runs");
    }
  }

  const rowList = $derived(
    [...rows.values()].sort((a, b) => a.id.localeCompare(b.id)),
  );

  const visibleRowList = $derived(
    rowList.filter((r) => r.status === "Enriched" || r.status === "Partial"),
  );

  let rememberedColumnKeys = $state<string[]>([...BASELINE_COLUMN_KEYS]);

  const discoveredColumnKeys = $derived.by(() => {
    const keys = new Set<string>();
    for (const r of visibleRowList) {
      if (r.row && typeof r.row === "object") {
        for (const k of Object.keys(r.row)) {
          if (k !== "_truncated") keys.add(k);
        }
      }
    }
    return [...keys].sort();
  });

  const columnKeys = $derived.by(() => {
    const merged = new Set<string>([...BASELINE_COLUMN_KEYS, ...rememberedColumnKeys, ...discoveredColumnKeys]);
    const sorted = [...merged].sort();
    const preferredPinned = ["ip", "mac"];
    const preferredUnpinnedLeft = ["hashrate", "average_temperature", "efficiency"];
    const pinned = preferredPinned.filter((key) => sorted.includes(key));
    const remaining = sorted.filter((key) => !preferredPinned.includes(key));
    const leftUnpinned = preferredUnpinnedLeft.filter((key) => remaining.includes(key));
    const rest = remaining.filter((key) => !preferredUnpinnedLeft.includes(key));
    return ["status", ...pinned, ...leftUnpinned, ...rest];
  });

  const gridRowData = $derived(
    visibleRowList.map((r) => ({
      id: r.id,
      status: r.status,
      ...(r.row ?? {}),
    })),
  );

  const gridColumnDefs = $derived.by<ColDef<GridRow>[]>(() =>
    columnKeys.map((col): ColDef<GridRow> => {
      if (col === "status") {
        return {
          field: col,
          headerName: formatColumnHeader(col),
          hide: true,
          width: 110,
          cellClass: (params) => {
            if (params.value === "Enriched") return "text-emerald-400 font-medium";
            if (params.value === "Partial") return "text-amber-400 font-medium";
            return "";
          },
        };
      }

      return {
        field: col,
        headerName: formatColumnHeader(col),
        pinned: col === "ip" || col === "mac" ? "left" : undefined,
        valueFormatter: (params: ValueFormatterParams<GridRow>) => {
          if (col === "hashrate") return formatHashRateValue(params.value);
          if (col === "average_temperature") {
            const value = params.value;
            if (value === undefined || value === null) return "\u2014";
            if (typeof value === "number" && Number.isFinite(value)) return `${Math.round(value)} \u00B0C`;
            if (typeof value === "string") {
              const trimmed = value.trim();
              const parsed = Number.parseFloat(trimmed);
              if (Number.isFinite(parsed)) return `${Math.round(parsed)} \u00B0C`;
              return trimmed;
            }
            return formatGridValue(value);
          }
          if (col === "efficiency") {
            const value = params.value;
            if (value === undefined || value === null) return "null";
            if (typeof value === "number" && Number.isFinite(value)) return `${value.toFixed(2)} J/TH`;
            if (typeof value === "string") {
              const trimmed = value.trim();
              const parsed = Number.parseFloat(trimmed);
              if (!Number.isFinite(parsed)) return "null";
              return `${parsed.toFixed(2)} J/TH`;
            }
            return "null";
          }
          return formatGridValue(params.value);
        },
      };
    }),
  );

  const defaultColDef: ColDef<GridRow> = {
    resizable: true,
    sortable: true,
    filter: true,
  };

  const probeProgressPercent = $derived.by(() => {
    if (progress.totalTargets <= 0) return busy ? 0 : 100;
    return Math.min(100, Math.max(0, (progress.probed / progress.totalTargets) * 100));
  });

  onMount(() => {
    settings = loadDiscoverySettings();
    targetProfiles = loadTargetProfiles();
    rememberedColumnKeys = loadGridColumnKeys();
    if (!gridHost) return;
    const gridOptions: GridOptions<GridRow> = {
      rowModelType: "clientSide",
      defaultColDef,
      animateRows: true,
      columnDefs: gridColumnDefs,
      rowData: gridRowData,
      localeText: {
        noRowsToShow: "No responding IPs yet",
      },
      getRowId: (params) => params.data.id,
    };
    gridApi = createGrid(gridHost, gridOptions);
    requestAnimationFrame(() => refreshGridLayout());
  });

  $effect(() => {
    saveTargetProfiles(targetProfiles);
  });

  $effect(() => {
    const merged = [...new Set([...rememberedColumnKeys, ...discoveredColumnKeys])].sort();
    if (
      merged.length === rememberedColumnKeys.length &&
      merged.every((key, idx) => key === rememberedColumnKeys[idx])
    ) {
      return;
    }
    rememberedColumnKeys = merged;
    saveGridColumnKeys(merged);
  });

  $effect(() => {
    if (!gridApi) return;
    gridApi.setGridOption("columnDefs", gridColumnDefs);
    gridApi.setGridOption("rowData", gridRowData);
    requestAnimationFrame(() => refreshGridLayout());
  });

  onDestroy(() => {
    for (const u of unlistenFns) void u();
    if (coalesceTimer) clearTimeout(coalesceTimer);
    gridApi?.destroy();
    gridApi = null;
  });
</script>

<div class="flex h-dvh w-full flex-col gap-3 p-4">
  <header class="flex flex-wrap items-end justify-between gap-3 pb-4">
    <div>
      <h1 class="text-xl font-semibold tracking-tight text-emerald-400">Terra Mine</h1>
    </div>
    <div class="flex flex-wrap items-end gap-2">
      <div class="flex flex-col gap-1">
        <label class="text-xs font-medium text-zinc-500" for="targets">Targets</label>
        <textarea
          id="targets"
          class="h-10 w-[24rem] max-w-[70vw] rounded-lg border border-zinc-800 bg-zinc-900 px-3 py-2 font-mono text-sm leading-tight"
          rows="1"
          bind:value={targets}
          placeholder="192.168.1.0/24 or 10.0.0.1, 10.0.0.5-10"
        ></textarea>
      </div>
      <div class="flex flex-wrap gap-2">
        <select
          class="h-10 rounded-lg border border-zinc-700 bg-zinc-900 px-2 py-1 text-xs text-zinc-300"
          value={selectedProfileId}
          onchange={(e) => applyTargetProfile((e.currentTarget as HTMLSelectElement).value)}
        >
          <option value="">Custom targets</option>
          {#each targetProfiles as profile}
            <option value={profile.id}>{profile.name}</option>
          {/each}
        </select>
        <button
          type="button"
          class="h-10 rounded-lg border border-zinc-700 px-2 py-1 text-xs text-zinc-300 hover:bg-zinc-900"
          onclick={saveTargetProfile}
        >
          {selectedProfileId ? "Update profile" : "Save profile"}
        </button>
        <button
          type="button"
          class="h-10 rounded-lg border border-zinc-700 px-2 py-1 text-xs text-zinc-300 hover:bg-zinc-900 disabled:opacity-40"
          disabled={!selectedProfileId}
          onclick={deleteTargetProfile}
        >
          Delete
        </button>
      </div>
      <a
        href="/settings"
        class="inline-flex h-10 w-10 items-center justify-center rounded-lg border border-zinc-600 text-zinc-300 hover:bg-zinc-900"
        aria-label="Run settings"
        title="Run settings"
      >
        <Settings class="h-5 w-5" aria-hidden="true" />
      </a>
      <button
        type="button"
        class="h-10 rounded-lg bg-emerald-600 px-4 py-2 text-sm font-medium text-white hover:bg-emerald-500 disabled:opacity-40"
        disabled={busy}
        onclick={startRun}
      >
        Discover
      </button>
      <button
        type="button"
        class="h-10 rounded-lg border border-zinc-600 px-4 py-2 text-sm hover:bg-zinc-900"
        onclick={stopRun}
      >
        Stop
      </button>
    </div>
  </header>

  {#if errorMsg}
    <div class="rounded-lg border border-red-900 bg-red-950/50 px-3 py-2 text-sm text-red-200">
      {errorMsg}
    </div>
  {/if}

  <div class="space-y-1">
    <div class="h-1.5 overflow-hidden rounded-full bg-zinc-800/90">
      <div
        class="h-full rounded-full bg-violet-500 shadow-[0_0_12px_rgba(139,92,246,0.55)] transition-[width] duration-300 ease-out"
        style={`width: ${probeProgressPercent}%;`}
      ></div>
    </div>
    <div class="flex flex-wrap gap-3 text-xs text-zinc-400">
      <span>Total {progress.totalTargets}</span>
      <span>Probed {progress.probed}</span>
      <span>Alive {progress.alive}</span>
      <span>Miss {progress.missed}</span>
      <span>Enriched {progress.enriched}</span>
      <span>Partial {progress.partial}</span>
      <span>Cancelled {progress.cancelled}</span>
    </div>
  </div>

  <div class="flex min-h-0 w-full flex-1 flex-col overflow-hidden rounded-xl border border-zinc-800">
    <div class="border-b border-zinc-800 px-3 py-2 text-sm text-zinc-400">Results</div>
    <div class="relative min-h-0 w-full flex-1">
      <div
        bind:this={gridHost}
        class="ag-theme-quartz-dark h-full w-full min-h-0 text-xs"
      ></div>
    </div>
  </div>
</div>
