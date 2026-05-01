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
  import { fade, fly } from "svelte/transition";
  import { onDestroy, onMount, untrack } from "svelte";
  import { Settings, X } from "lucide-svelte";
  import {
    defaultDiscoverySettings,
    type DiscoveryProgress,
    type DiscoveryRowEvent,
    type DiscoverySettings,
  } from "$lib/discoveryDefaults";
  import {
    clearKnownDevices,
    DEFAULT_PROFILE_ID,
    loadDiscoverySettings,
    loadKnownDevices,
    loadTargetProfiles,
    normalizeTargetProfiles,
    saveKnownDevices,
    saveTargetProfiles,
    type KnownDevice,
    type TargetProfile,
  } from "$lib/discoverySettingsStorage";

  let targets = $state("192.168.1.0/24");
  let settings = $state<DiscoverySettings>({ ...defaultDiscoverySettings });
  let runId = $state<string | null>(null);
  let busy = $state(false);
  let errorMsg = $state<string | null>(null);
  let activeRunKind = $state<"discover" | "enrich" | null>(null);

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
  let selectedProfileId = $state(DEFAULT_PROFILE_ID);
  let profileDrawerOpen = $state(false);
  /** `null` = new profile in drawer; otherwise id of profile being edited */
  let profileDrawerEditingId = $state<string | null>(null);
  let drawerProfileName = $state("");
  let drawerProfileTargets = $state("");
  let profileDrawerError = $state<string | null>(null);
  /** Tauri/WebKit often breaks `confirm()`; confirm deletes in-drawer instead. */
  let profileDeleteConfirmPending = $state(false);
  /** Header "Clear devices" avoids `window.confirm`. */
  let clearDevicesConfirmPending = $state(false);
  let knownDevices = $state<KnownDevice[]>([]);
  /** Blocks LocalStorage saves until persisted lists are hydrated (avoids wiping with initial []). */
  let discoveryListsPersistAllowed = $state(false);

  let unlistenFns: UnlistenFn[] = [];
  let coalesceTimer: ReturnType<typeof setTimeout> | null = null;
  let pendingRows = new Map<string, DiscoveryRowEvent>();
  let missStreakByRow = $state<Map<string, number>>(new Map());
  let gridHost: HTMLDivElement | null = null;
  let gridApi: GridApi<GridRow> | null = null;
  let autoDiscoverTimer: ReturnType<typeof setInterval> | null = null;
  let autoEnrichTimer: ReturnType<typeof setInterval> | null = null;
  /** Bumped when the active target profile/spec changes so auto-discover/enrich timers reset their interval clocks. */
  let autoLoopRestartEpoch = $state(0);
  let movementByCell = $state<Map<string, { direction: "up" | "down"; deltaText: string }>>(new Map());

  type GridRow = {
    id: string;
    status: DiscoveryRowEvent["status"];
    [key: string]: unknown;
  };
  const MOVEMENT_COLUMNS = new Set(["hashrate", "average_temperature", "efficiency", "wattage"]);
  const GRID_COLUMN_KEYS_STORAGE_KEY = "tm:discovery:grid-column-keys:v1";
  const BASELINE_COLUMN_KEYS = ["ip", "discovery"];

  function isFinalStatus(status: DiscoveryRowEvent["status"]) {
    return status === "Enriched" || status === "Partial";
  }

  function rowKeyForEvent(ev: DiscoveryRowEvent) {
    const ip = typeof ev.row?.ip === "string" ? ev.row.ip.trim() : "";
    return ip || ev.id;
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

  const HASH_RATE_UNIT_TIERS = ["H/s", "KH/s", "MH/s", "GH/s", "TH/s", "PH/s", "EH/s", "ZH/s"] as const;

  const HASH_RATE_UNIT_MAP: Record<string, string> = {
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

  /** Same tier logic as displayed hashrate cells; used for primary value and delta-in-that-unit. */
  function scaleHashRateToDisplayTier(amount: number, unit: string) {
    const unitTiers = HASH_RATE_UNIT_TIERS;
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
  }

  /** Parsed display tier for a cell value (matches `formatHashRateValue`). */
  function getNormalizedHashRateDisplay(value: unknown): { amount: number; unit: string } | null {
    if (value === undefined || value === null) return null;
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
      if (!Number.isFinite(amount)) return null;
      const unitKey =
        typeof rawUnit === "string" ? rawUnit.trim().toLowerCase().replace(/\s+/g, "") : "";
      const unit = HASH_RATE_UNIT_MAP[unitKey] ?? (typeof rawUnit === "string" ? rawUnit.trim() : "");
      if (!unit) return scaleHashRateToDisplayTier(Math.max(0, amount), "H/s");
      return scaleHashRateToDisplayTier(amount, unit);
    }
    if (typeof value === "number" && Number.isFinite(value)) {
      return scaleHashRateToDisplayTier(value, "H/s");
    }
    if (typeof value === "string") {
      const trimmed = value.trim();
      const match = trimmed.match(/^(-?\d+(?:\.\d+)?)(.*)$/);
      if (!match) return null;
      const amount = Number.parseFloat(match[1]);
      if (!Number.isFinite(amount)) return null;
      const rawUnit = match[2].trim();
      const unitKey = rawUnit.toLowerCase().replace(/\s+/g, "");
      const unit = HASH_RATE_UNIT_MAP[unitKey] ?? rawUnit;
      return scaleHashRateToDisplayTier(amount, unit);
    }
    return null;
  }

  /** Delta is in H/s; scale to same magnitude as the reference cell (no unit label in the indicator). */
  function formatHashRateDeltaHs(absDeltaHs: number, referenceValue: unknown): string {
    if (!Number.isFinite(absDeltaHs)) return "\u2014";
    const refDisplay = getNormalizedHashRateDisplay(referenceValue);
    if (!refDisplay) {
      const scaled = scaleHashRateToDisplayTier(absDeltaHs, "H/s");
      return scaled.amount.toFixed(2);
    }
    const idx = HASH_RATE_UNIT_TIERS.indexOf(refDisplay.unit as (typeof HASH_RATE_UNIT_TIERS)[number]);
    if (idx < 0) {
      const scaled = scaleHashRateToDisplayTier(absDeltaHs, "H/s");
      return scaled.amount.toFixed(2);
    }
    const factor = 1000 ** idx;
    return (absDeltaHs / factor).toFixed(2);
  }

  function parseHashRateToHs(value: unknown): number | null {
    if (value === undefined || value === null) return null;
    const unitFactor: Record<string, number> = {
      "h/s": 1,
      hs: 1,
      h: 1,
      hash: 1,
      khs: 1e3,
      "kh/s": 1e3,
      kh: 1e3,
      kilohash: 1e3,
      mhs: 1e6,
      "mh/s": 1e6,
      mh: 1e6,
      megahash: 1e6,
      ghs: 1e9,
      "gh/s": 1e9,
      gh: 1e9,
      gigahash: 1e9,
      ths: 1e12,
      "th/s": 1e12,
      th: 1e12,
      terahash: 1e12,
      phs: 1e15,
      "ph/s": 1e15,
      ph: 1e15,
      petahash: 1e15,
      ehs: 1e18,
      "eh/s": 1e18,
      eh: 1e18,
      exahash: 1e18,
      zhs: 1e21,
      "zh/s": 1e21,
      zh: 1e21,
      zettahash: 1e21,
    };
    if (typeof value === "number" && Number.isFinite(value)) return value;
    if (typeof value === "string") {
      const trimmed = value.trim();
      const match = trimmed.match(/^(-?\d+(?:\.\d+)?)(.*)$/);
      if (!match) return null;
      const amount = Number.parseFloat(match[1]);
      if (!Number.isFinite(amount)) return null;
      const unit = match[2].trim().toLowerCase().replace(/\s+/g, "");
      const factor = unitFactor[unit] ?? 1;
      return amount * factor;
    }
    if (typeof value === "object") {
      const record = value as Record<string, unknown>;
      const rawAmount = record.value;
      const amount =
        typeof rawAmount === "number"
          ? rawAmount
          : typeof rawAmount === "string"
            ? Number.parseFloat(rawAmount)
            : Number.NaN;
      if (!Number.isFinite(amount)) return null;
      const rawUnit = typeof record.unit === "string" ? record.unit.trim().toLowerCase().replace(/\s+/g, "") : "";
      const factor = unitFactor[rawUnit] ?? 1;
      return amount * factor;
    }
    return null;
  }

  function parseMetricValue(col: string, value: unknown): number | null {
    if (!MOVEMENT_COLUMNS.has(col)) return null;
    if (col === "hashrate") return parseHashRateToHs(value);
    if (typeof value === "number" && Number.isFinite(value)) return value;
    if (typeof value === "string") {
      const parsed = Number.parseFloat(value.trim());
      return Number.isFinite(parsed) ? parsed : null;
    }
    return null;
  }

  function formatColumnValue(col: string, value: unknown): string {
    if (col === "hashrate") return formatHashRateValue(value);
    if (col === "average_temperature") {
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
      if (value === undefined || value === null) return "null";
      if (typeof value === "number" && Number.isFinite(value)) return `${value.toFixed(2)} J/TH`;
      if (typeof value === "string") {
        const parsed = Number.parseFloat(value.trim());
        if (!Number.isFinite(parsed)) return "null";
        return `${parsed.toFixed(2)} J/TH`;
      }
      return "null";
    }
    if (col === "wattage") {
      if (value === undefined || value === null) return "\u2014";
      if (typeof value === "number" && Number.isFinite(value)) return `${value.toFixed(2)} W`;
      if (typeof value === "string") {
        const parsed = Number.parseFloat(value.trim());
        if (Number.isFinite(parsed)) return `${parsed.toFixed(2)} W`;
        return value;
      }
      return formatGridValue(value);
    }
    return formatGridValue(value);
  }

  function formatDelta(col: string, absDelta: number): string {
    if (col === "average_temperature") return String(Math.round(absDelta));
    if (col === "efficiency") return absDelta.toFixed(2);
    if (col === "wattage") return absDelta.toFixed(2);
    return absDelta.toFixed(2);
  }

  function movementKey(rowId: string, col: string) {
    return `${rowId}:${col}`;
  }

  function formatGridValue(value: unknown) {
    if (value === undefined) return "\u2014";
    if (typeof value === "string") return value;
    return JSON.stringify(value);
  }

  function formatHashRateValue(value: unknown) {
    if (value === undefined || value === null) return "\u2014";
    const scaled = getNormalizedHashRateDisplay(value);
    if (!scaled) return formatGridValue(value);
    return `${scaled.amount.toFixed(2)} ${scaled.unit}`;
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

  function bumpAutoLoopsForProfileScopeChange() {
    autoLoopRestartEpoch += 1;
  }

  /** Stop any in-flight discover/enrich run and detach listeners after backend cancel (profile switch). */
  async function interruptAllDiscoveryRunsForUi() {
    try {
      await invoke<number>("stop_all_discovery_runs");
    } catch {
      // ignore
    }
    for (const u of unlistenFns) void u();
    unlistenFns = [];
    if (coalesceTimer) clearTimeout(coalesceTimer);
    coalesceTimer = null;
    pendingRows.clear();
    busy = false;
    runId = null;
    activeRunKind = null;
    flushRows();
  }

  async function pruneStateToCurrentTargetsSpec() {
    const spec = targets.trim();
    if (!spec) return;

    const candidates = new Set<string>();
    for (const d of knownDevices) {
      const ip = d.ip.trim();
      if (ip) candidates.add(ip);
    }
    for (const ev of rows.values()) {
      const ip = typeof ev.row?.ip === "string" ? ev.row.ip.trim() : "";
      if (ip) candidates.add(ip);
    }
    const list = [...candidates];
    if (list.length === 0) return;

    try {
      const kept = await invoke<string[]>("filter_ips_matching_targets", {
        ips: list,
        targets: spec,
      });
      const keptSet = new Set(kept.map((s) => s.trim()));

      knownDevices = knownDevices.filter((d) => keptSet.has(d.ip.trim()));

      const nextRows = new Map(rows);
      for (const [k, ev] of rows) {
        const ip = typeof ev.row?.ip === "string" ? ev.row.ip.trim() : "";
        if (ip && !keptSet.has(ip)) nextRows.delete(k);
      }
      rows = nextRows;

      for (const k of [...pendingRows.keys()]) {
        if (!keptSet.has(k)) pendingRows.delete(k);
      }

      const nextMiss = new Map(missStreakByRow);
      for (const rk of [...nextMiss.keys()]) {
        if (!keptSet.has(rk)) nextMiss.delete(rk);
      }
      missStreakByRow = nextMiss;

      const nextMovement = new Map(movementByCell);
      for (const key of [...nextMovement.keys()]) {
        const idx = key.lastIndexOf(":");
        const rowId = idx > 0 ? key.slice(0, idx) : "";
        if (rowId && !keptSet.has(rowId)) nextMovement.delete(key);
      }
      movementByCell = nextMovement;
    } catch (e) {
      console.warn("prune targets: filter_ips_matching_targets failed", e);
    }
  }

  /** After profile/target scope changes: cancel runs, prune, reset auto-interval clocks, optionally start discover immediately. */
  async function handleProfileTargetsScopeChanged() {
    await interruptAllDiscoveryRunsForUi();
    bumpAutoLoopsForProfileScopeChange();
    await pruneStateToCurrentTargetsSpec();
    if (settings.autoDiscoverEnabled && targets.trim()) {
      queueMicrotask(() => void startRun());
    }
  }

  async function applyTargetProfile(profileId: string) {
    const id = profileId || DEFAULT_PROFILE_ID;
    selectedProfileId = id;
    const profile = targetProfiles.find((p) => p.id === id);
    if (profile) {
      targets = profile.targets;
    }
    await handleProfileTargetsScopeChanged();
  }

  function closeProfileDrawer() {
    profileDrawerOpen = false;
    profileDrawerError = null;
    profileDrawerEditingId = null;
    profileDeleteConfirmPending = false;
  }

  function openProfileDrawerNew() {
    profileDrawerEditingId = null;
    drawerProfileName = "";
    drawerProfileTargets = targets.trim();
    profileDrawerError = null;
    profileDeleteConfirmPending = false;
    profileDrawerOpen = true;
  }

  function openProfileDrawerEdit() {
    const p = targetProfiles.find((x) => x.id === selectedProfileId);
    if (!p) return;
    profileDrawerEditingId = p.id;
    drawerProfileName = p.name;
    drawerProfileTargets = p.targets;
    profileDrawerError = null;
    profileDeleteConfirmPending = false;
    profileDrawerOpen = true;
  }

  function startProfileDeleteConfirmation() {
    const id = profileDrawerEditingId;
    if (!id || id === DEFAULT_PROFILE_ID) return;
    profileDrawerError = null;
    profileDeleteConfirmPending = true;
  }

  function cancelProfileDeleteConfirmation() {
    profileDeleteConfirmPending = false;
  }

  function normalizeProfileNameKey(name: string) {
    return name.trim().toLowerCase();
  }

  function profileNameTakenByOther(
    ignoringId: string | null,
    name: string,
  ): string | null {
    const nk = normalizeProfileNameKey(name);
    for (const p of targetProfiles) {
      if (ignoringId && p.id === ignoringId) continue;
      if (normalizeProfileNameKey(p.name) === nk) return "Another profile already uses this name.";
    }
    return null;
  }

  async function saveProfileFromDrawer() {
    profileDrawerError = null;
    const name = drawerProfileName.trim();
    const t = drawerProfileTargets.trim();
    if (!name) {
      profileDrawerError = "Enter a profile name.";
      return;
    }
    if (!t) {
      profileDrawerError = "Enter at least one target (CIDR, IP, or range).";
      return;
    }
    const dup = profileNameTakenByOther(profileDrawerEditingId, name);
    if (dup) {
      profileDrawerError = dup;
      return;
    }
    const editingId = profileDrawerEditingId;
    if (editingId) {
      targetProfiles = normalizeTargetProfiles(
        targetProfiles.map((p) =>
          p.id === editingId ? { ...p, name, targets: t } : p,
        ),
      );
      if (selectedProfileId === editingId) {
        targets = t;
      }
    } else {
      const id =
        typeof crypto !== "undefined" && "randomUUID" in crypto ? crypto.randomUUID() : `${Date.now()}`;
      targetProfiles = normalizeTargetProfiles([...targetProfiles, { id, name, targets: t }]);
      selectedProfileId = id;
      targets = t;
    }
    await handleProfileTargetsScopeChanged();
    closeProfileDrawer();
  }

  function confirmDeleteProfileFromDrawer() {
    const id = profileDrawerEditingId;
    if (!id || id === DEFAULT_PROFILE_ID) return;
    targetProfiles = normalizeTargetProfiles(targetProfiles.filter((p) => p.id !== id));
    if (selectedProfileId === id) {
      selectedProfileId = DEFAULT_PROFILE_ID;
      const def = targetProfiles.find((p) => p.id === DEFAULT_PROFILE_ID);
      if (def) targets = def.targets;
    }
    profileDeleteConfirmPending = false;
    closeProfileDrawer();
  }

  function normalizeKnownDevices(devices: KnownDevice[]) {
    const byIp = new Map<string, KnownDevice>();
    for (const device of devices) {
      const ip = device.ip.trim();
      if (!ip) continue;
      const existing = byIp.get(ip);
      byIp.set(ip, {
        ip,
        mac: device.mac?.trim() || existing?.mac,
        lastSeenAt: Math.max(existing?.lastSeenAt ?? 0, device.lastSeenAt),
      });
    }
    return [...byIp.values()].sort((a, b) => a.ip.localeCompare(b.ip));
  }

  function executeClearKnownDevices() {
    knownDevices = [];
    clearKnownDevices();
    pendingRows.clear();
    rows = new Map();
    missStreakByRow = new Map();
    movementByCell = new Map();
    progress = { ...emptyProgress };
    clearDevicesConfirmPending = false;
  }

  function cancelClearKnownDevicesConfirm() {
    clearDevicesConfirmPending = false;
  }

  function startClearKnownDevicesConfirm() {
    if (knownDevices.length === 0) return;
    clearDevicesConfirmPending = true;
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
    const key = rowKeyForEvent(ev);
    const normalizedEvent = key === ev.id ? ev : { ...ev, id: key };
    if (activeRunKind === "discover" && normalizedEvent.status === "Miss") {
      const nextStreak = (missStreakByRow.get(key) ?? 0) + 1;
      const nextMissStreak = new Map(missStreakByRow);
      nextMissStreak.set(key, nextStreak);
      missStreakByRow = nextMissStreak;
      if (nextStreak >= 3) {
        pendingRows.delete(key);
        const nextRows = new Map(rows);
        nextRows.delete(key);
        rows = nextRows;
        const nextMovement = new Map(movementByCell);
        for (const col of MOVEMENT_COLUMNS) {
          nextMovement.delete(movementKey(key, col));
        }
        movementByCell = nextMovement;
        const resetMissStreak = new Map(missStreakByRow);
        resetMissStreak.delete(key);
        missStreakByRow = resetMissStreak;
      }
      return;
    }
    if (isFinalStatus(ev.status)) {
      // Show finalized discovery rows immediately when enrichment completes.
      pendingRows.delete(key);
      const previous = rows.get(key);
      rows = new Map(rows).set(key, normalizedEvent);
      if (missStreakByRow.has(key)) {
        const nextMissStreak = new Map(missStreakByRow);
        nextMissStreak.delete(key);
        missStreakByRow = nextMissStreak;
      }
      const currentRow = normalizedEvent.row ?? {};
      const previousRow = previous?.row ?? {};
      const nextMovement = new Map(movementByCell);
      for (const col of MOVEMENT_COLUMNS) {
        const prevMetric = parseMetricValue(col, previousRow[col]);
        const nextMetric = parseMetricValue(col, currentRow[col]);
        const keyForCell = movementKey(key, col);
        if (prevMetric === null || nextMetric === null) {
          nextMovement.delete(keyForCell);
          continue;
        }
        const delta = nextMetric - prevMetric;
        if (!Number.isFinite(delta) || Math.abs(delta) < 0.000001) {
          nextMovement.delete(keyForCell);
          continue;
        }
        nextMovement.set(keyForCell, {
          direction: delta > 0 ? "up" : "down",
          deltaText:
            col === "hashrate"
              ? formatHashRateDeltaHs(Math.abs(delta), currentRow[col])
              : formatDelta(col, Math.abs(delta)),
        });
      }
      movementByCell = nextMovement;
      return;
    }
    const existing = rows.get(key);
    if (existing && isFinalStatus(existing.status)) return;
    pendingRows.set(key, normalizedEvent);
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
          if (activeRunKind !== "discover") return;
          progress = e.payload;
        }),
      );
      unlistenFns.push(
        await listen(doneEv, () => {
          busy = false;
          runId = null;
          activeRunKind = null;
          flushRows();
        }),
      );
      unlistenFns.push(
        await listen<{ code: string; message: string }>(errEv, (e) => {
          errorMsg = `${e.payload.code}: ${e.payload.message}`;
          busy = false;
          activeRunKind = null;
        }),
      );
    } catch (e) {
      throw e;
    }
  }

  async function startRun() {
    if (activeRunKind) return;
    errorMsg = null;
    progress = { ...emptyProgress };
    pendingRows.clear();
    busy = true;
    activeRunKind = "discover";
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
      activeRunKind = null;
      errorMsg = String(e);
    }
  }

  async function startKnownDeviceEnrichRun() {
    if (busy || knownDevices.length === 0) return;
    errorMsg = null;
    pendingRows.clear();
    busy = true;
    activeRunKind = "enrich";
    const id =
      typeof crypto !== "undefined" && "randomUUID" in crypto
        ? crypto.randomUUID()
        : `${Date.now()}-${Math.random().toString(16).slice(2)}`;
    try {
      await wireEvents(id);
      await invoke<string>("enrich_known_devices_run", {
        runId: id,
        devices: knownDevices.map((d) => d.ip),
        settings,
      });
      runId = id;
    } catch (e) {
      busy = false;
      activeRunKind = null;
      errorMsg = String(e);
    }
  }

  async function stopRun() {
    if (!runId) {
      try {
        await invoke<number>("stop_all_discovery_runs");
        busy = false;
        activeRunKind = null;
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
    const preferredUnpinnedLeft = ["hashrate", "average_temperature", "efficiency", "wattage"];
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
            if (params.value === "Enriched") return "tm-accent-text font-medium";
            if (params.value === "Partial") return "text-amber-400 font-medium";
            return "";
          },
        };
      }

      return {
        field: col,
        headerName: formatColumnHeader(col),
        pinned: col === "ip" || col === "mac" ? "left" : undefined,
        valueFormatter: (params: ValueFormatterParams<GridRow>) => formatColumnValue(col, params.value),
        cellRenderer: (params: ValueFormatterParams<GridRow>) => {
          const formatted = formatColumnValue(col, params.value);
          if (!MOVEMENT_COLUMNS.has(col)) return formatted;
          const rowId = typeof params.data?.id === "string" ? params.data.id : "";
          if (!rowId) return formatted;
          const movement = movementByCell.get(movementKey(rowId, col));
          if (!movement) return formatted;
          const wrapper = document.createElement("span");
          wrapper.className = "tm-move-cell";
          const valueEl = document.createElement("span");
          valueEl.className = "tm-move-value";
          valueEl.textContent = formatted;
          const indicator = document.createElement("span");
          // average_temperature: up = bad (orange), down = good (ice blue).
          // efficiency: lower J/TH is better — invert green/red (up = bad, down = good).
          let colorClass: string;
          if (col === "average_temperature") {
            colorClass = movement.direction === "up" ? "tm-move-temp-up" : "tm-move-temp-down";
          } else if (col === "efficiency") {
            colorClass =
              movement.direction === "up"
                ? "tm-move-down"
                : "tm-move-up";
          } else {
            colorClass = movement.direction === "up" ? "tm-move-up" : "tm-move-down";
          }
          indicator.className = `tm-move-indicator ${colorClass}`;
          indicator.textContent = `${movement.direction === "up" ? "↑" : "↓"} ${movement.direction === "up" ? "+" : "-"}${movement.deltaText}`;
          wrapper.append(valueEl, indicator);
          return wrapper;
        },
        comparator: (valueA, valueB) => {
          if (MOVEMENT_COLUMNS.has(col)) {
            const a = parseMetricValue(col, valueA);
            const b = parseMetricValue(col, valueB);
            if (a === null && b === null) return 0;
            if (a === null) return -1;
            if (b === null) return 1;
            return a - b;
          }
          return String(valueA ?? "").localeCompare(String(valueB ?? ""));
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
    if (progress.totalTargets <= 0) return activeRunKind === "discover" ? 0 : 100;
    return Math.min(100, Math.max(0, (progress.probed / progress.totalTargets) * 100));
  });
  const discoverInProgress = $derived(activeRunKind === "discover");

  onMount(() => {
    settings = loadDiscoverySettings();
    targetProfiles = normalizeTargetProfiles(loadTargetProfiles());
    const def = targetProfiles.find((p) => p.id === DEFAULT_PROFILE_ID);
    if (def) {
      targets = def.targets;
      selectedProfileId = DEFAULT_PROFILE_ID;
    }
    knownDevices = loadKnownDevices();
    rememberedColumnKeys = loadGridColumnKeys();
    discoveryListsPersistAllowed = true;
    if (!gridHost) return;
    const gridOptions: GridOptions<GridRow> = {
      rowModelType: "clientSide",
      defaultColDef,
      animateRows: true,
      columnDefs: gridColumnDefs,
      rowData: gridRowData,
      onModelUpdated: () => {
        if (!gridApi) return;
        refreshGridLayout();
      },
      localeText: {
        noRowsToShow: "No responding IPs yet",
      },
      getRowId: (params) => params.data.id,
    };
    gridApi = createGrid(gridHost, gridOptions);
    requestAnimationFrame(() => refreshGridLayout());
  });

  $effect(() => {
    if (!discoveryListsPersistAllowed) return;
    saveTargetProfiles(targetProfiles);
  });

  /** Keep the selected profile's stored targets in sync with the main textarea (avoids losing edits on Default). */
  $effect(() => {
    if (!discoveryListsPersistAllowed) return;
    const id = selectedProfileId;
    const t = targets;
    untrack(() => {
      const cur = targetProfiles.find((p) => p.id === id);
      if (!cur || cur.targets === t) return;
      targetProfiles = targetProfiles.map((p) => (p.id === id ? { ...p, targets: t } : p));
    });
  });

  $effect(() => {
    if (!discoveryListsPersistAllowed) return;
    saveKnownDevices(knownDevices);
  });

  $effect(() => {
    if (!profileDrawerOpen || typeof window === "undefined") return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") closeProfileDrawer();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
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
    if (visibleRowList.length === 0) return;
    const next = [...knownDevices];
    let changed = false;
    for (const row of visibleRowList) {
      const ip = typeof row.row?.ip === "string" ? row.row.ip.trim() : "";
      if (!ip) continue;
      const mac = typeof row.row?.mac === "string" ? row.row.mac.trim() : undefined;
      const existingIdx = next.findIndex((d) => d.ip === ip);
      if (existingIdx >= 0) {
        const existing = next[existingIdx];
        const resolvedMac = mac || existing.mac;
        if (resolvedMac !== existing.mac) {
          next[existingIdx] = {
            ...existing,
            mac: resolvedMac,
            lastSeenAt: Date.now(),
          };
          changed = true;
        }
      } else {
        next.push({
          ip,
          mac,
          lastSeenAt: Date.now(),
        });
        changed = true;
      }
    }
    if (changed) {
      knownDevices = normalizeKnownDevices(next);
    }
  });

  $effect(() => {
    autoLoopRestartEpoch;
    if (autoDiscoverTimer) {
      clearInterval(autoDiscoverTimer);
      autoDiscoverTimer = null;
    }
    if (!settings.autoDiscoverEnabled) return;
    const intervalMs = Math.max(5, settings.autoDiscoverIntervalSec) * 1000;
    autoDiscoverTimer = setInterval(() => {
      if (busy) return;
      void startRun();
    }, intervalMs);
    return () => {
      if (autoDiscoverTimer) {
        clearInterval(autoDiscoverTimer);
        autoDiscoverTimer = null;
      }
    };
  });

  $effect(() => {
    autoLoopRestartEpoch;
    if (autoEnrichTimer) {
      clearInterval(autoEnrichTimer);
      autoEnrichTimer = null;
    }
    if (!settings.autoEnrichEnabled) return;
    const intervalMs = Math.max(5, settings.autoEnrichIntervalSec) * 1000;
    autoEnrichTimer = setInterval(() => {
      if (busy || knownDevices.length === 0) return;
      void startKnownDeviceEnrichRun();
    }, intervalMs);
    return () => {
      if (autoEnrichTimer) {
        clearInterval(autoEnrichTimer);
        autoEnrichTimer = null;
      }
    };
  });

  $effect(() => {
    if (!gridApi) return;
    const api = gridApi;
    api.setGridOption("columnDefs", gridColumnDefs);
    api.setGridOption("rowData", gridRowData);
    requestAnimationFrame(() => {
      if (gridApi !== api) return;
      refreshGridLayout();
    });
  });

  onDestroy(() => {
    for (const u of unlistenFns) void u();
    if (coalesceTimer) clearTimeout(coalesceTimer);
    if (autoDiscoverTimer) clearInterval(autoDiscoverTimer);
    if (autoEnrichTimer) clearInterval(autoEnrichTimer);
    gridApi?.destroy();
    gridApi = null;
  });
</script>

<div class="flex h-dvh w-full flex-col gap-3 p-4">
  <header class="flex flex-wrap items-end justify-between gap-3 pb-4">
    <div class="flex items-center gap-2">
      <img src="/terra-mine-logo.png" alt="TerraMine logo" class="h-7 w-7 object-contain" />
      <h1 class="text-lg font-semibold tracking-tight tm-accent-text">TerraMine</h1>
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
      <div class="flex flex-wrap items-end gap-2">
        <select
          class="h-10 min-w-[10rem] rounded-lg border border-zinc-700 bg-zinc-900 px-2 py-1 text-xs text-zinc-300"
          value={selectedProfileId}
          onchange={(e) => void applyTargetProfile((e.currentTarget as HTMLSelectElement).value)}
        >
          {#each targetProfiles as profile}
            <option value={profile.id}>{profile.name}</option>
          {/each}
        </select>
        <button
          type="button"
          class="h-10 rounded-lg border border-zinc-700 bg-zinc-900 px-3 py-1 text-xs text-zinc-200 hover:bg-zinc-800"
          onclick={openProfileDrawerNew}
        >
          + Profile
        </button>
        <button
          type="button"
          class="h-10 rounded-lg border border-zinc-700 px-2 py-1 text-xs text-zinc-300 hover:bg-zinc-900"
          onclick={openProfileDrawerEdit}
          title="Edit the selected profile"
        >
          Edit
        </button>
        {#if clearDevicesConfirmPending}
          <span class="self-center text-xs text-amber-300/95">Clear saved devices &amp; table?</span>
          <button
            type="button"
            class="h-10 shrink-0 rounded-lg border border-zinc-600 px-2 py-1 text-xs text-zinc-300 hover:bg-zinc-900"
            onclick={cancelClearKnownDevicesConfirm}
          >
            Cancel
          </button>
          <button
            type="button"
            class="h-10 shrink-0 rounded-lg border border-red-800 bg-red-900/55 px-2 py-1 text-xs text-red-100 hover:bg-red-900/80"
            onclick={executeClearKnownDevices}
          >
            Confirm clear
          </button>
        {:else}
          <button
            type="button"
            class="h-10 rounded-lg border border-zinc-700 px-2 py-1 text-xs text-zinc-300 hover:bg-zinc-900 disabled:opacity-40"
            disabled={knownDevices.length === 0}
            onclick={startClearKnownDevicesConfirm}
            title="Clear persisted known devices and remove rows from Results"
          >
            Clear devices ({knownDevices.length})
          </button>
        {/if}
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
        class="h-10 rounded-lg tm-accent-bg px-4 py-2 text-sm font-medium text-black hover:text-black disabled:opacity-40"
        disabled={discoverInProgress}
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

  {#if profileDrawerOpen}
    <div
      class="fixed inset-0 z-[100] bg-black/60"
      role="presentation"
      aria-hidden="true"
      onclick={closeProfileDrawer}
      transition:fade={{ duration: 150 }}
    ></div>
    <div
      class="fixed top-0 right-0 z-[101] flex h-full w-full max-w-md flex-col border-l border-zinc-800 bg-zinc-950 shadow-2xl"
      role="dialog"
      aria-modal="true"
      aria-labelledby="profile-drawer-title"
      transition:fly={{ x: 400, duration: 220, opacity: 1 }}
    >
      <div class="flex items-center justify-between border-b border-zinc-800 px-4 py-3">
        <h2 id="profile-drawer-title" class="text-base font-semibold text-zinc-100">
          {profileDrawerEditingId ? "Edit profile" : "New profile"}
        </h2>
        <button
          type="button"
          class="rounded-lg p-1.5 text-zinc-400 hover:bg-zinc-800 hover:text-zinc-100"
          onclick={closeProfileDrawer}
          aria-label="Close profile panel"
        >
          <X class="h-5 w-5" aria-hidden="true" />
        </button>
      </div>
      <div class="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto px-4 py-4">
        <div class="flex flex-col gap-1.5">
          <label class="text-xs font-medium text-zinc-500" for="drawer-profile-name">Name</label>
          <input
            id="drawer-profile-name"
            type="text"
            bind:value={drawerProfileName}
            class="rounded-lg border border-zinc-700 bg-zinc-900 px-3 py-2 text-sm text-zinc-200 placeholder:text-zinc-600 focus:border-zinc-500 focus:outline-none"
            placeholder="e.g. Home LAN"
            oninput={() => {
              profileDrawerError = null;
              profileDeleteConfirmPending = false;
            }}
          />
        </div>
        <div class="flex min-h-0 flex-1 flex-col gap-1.5">
          <label class="text-xs font-medium text-zinc-500" for="drawer-profile-targets">Targets</label>
          <textarea
            id="drawer-profile-targets"
            bind:value={drawerProfileTargets}
            class="min-h-[8rem] flex-1 resize-y rounded-lg border border-zinc-700 bg-zinc-900 px-3 py-2 font-mono text-sm text-zinc-200 placeholder:text-zinc-600 focus:border-zinc-500 focus:outline-none"
            placeholder="192.168.1.0/24 or 10.0.0.1, 10.0.0.5-10"
            rows="8"
            oninput={() => {
              profileDrawerError = null;
              profileDeleteConfirmPending = false;
            }}
          ></textarea>
        </div>
        {#if profileDrawerError}
          <p class="text-xs text-red-400" role="alert">{profileDrawerError}</p>
        {/if}
      </div>
      <div class="mt-auto flex w-full flex-col gap-3 border-t border-zinc-800 px-4 py-3">
        {#if profileDrawerEditingId && profileDrawerEditingId !== DEFAULT_PROFILE_ID && profileDeleteConfirmPending}
          <div class="rounded-lg border border-red-900/60 bg-red-950/35 px-3 py-2 text-xs">
            <p class="font-medium text-red-200">Delete this profile?</p>
            <p class="mt-1 text-red-300/90">You cannot undo this.</p>
          </div>
        {/if}
        <div class="flex w-full flex-wrap items-center gap-2">
          <div class="flex min-w-0 flex-1 flex-wrap items-center gap-2">
            {#if profileDrawerEditingId && profileDrawerEditingId !== DEFAULT_PROFILE_ID}
              {#if profileDeleteConfirmPending}
                <button
                  type="button"
                  class="h-10 shrink-0 rounded-lg border border-zinc-600 px-3 text-xs text-zinc-200 hover:bg-zinc-800"
                  onclick={cancelProfileDeleteConfirmation}
                >
                  Back
                </button>
                <button
                  type="button"
                  class="h-10 shrink-0 rounded-lg border border-red-800 bg-red-900/70 px-3 text-xs text-red-100 hover:bg-red-900"
                  onclick={confirmDeleteProfileFromDrawer}
                >
                  Delete permanently
                </button>
              {:else}
                <button
                  type="button"
                  class="h-10 shrink-0 rounded-lg border border-red-900/80 bg-red-950/40 px-3 text-xs text-red-300 hover:bg-red-950/70"
                  onclick={startProfileDeleteConfirmation}
                >
                  Delete profile
                </button>
              {/if}
            {/if}
          </div>
          <div class="flex shrink-0 flex-wrap items-center justify-end gap-2">
            <button
              type="button"
              class="h-10 rounded-lg border border-zinc-600 px-3 text-xs text-zinc-300 hover:bg-zinc-800"
              onclick={closeProfileDrawer}
            >
              Cancel
            </button>
            <button
              type="button"
              class="h-10 rounded-lg px-4 text-xs font-medium text-black tm-accent-bg hover:text-black"
              onclick={() => void saveProfileFromDrawer()}
            >
              Save
            </button>
          </div>
        </div>
      </div>
    </div>
  {/if}
</div>
