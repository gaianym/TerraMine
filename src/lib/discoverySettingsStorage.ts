import { defaultDiscoverySettings, type DiscoverySettings } from "$lib/discoveryDefaults";

const STORAGE_KEY = "tm.discovery.settings.v1";
const TARGET_PROFILES_KEY = "tm.discovery.targetProfiles.v1";
const KNOWN_DEVICES_KEY = "tm.discovery.knownDevices.v1";

export type TargetProfile = {
  id: string;
  name: string;
  targets: string;
};

export type KnownDevice = {
  ip: string;
  mac?: string;
  lastSeenAt: number;
};

export function loadDiscoverySettings(): DiscoverySettings {
  if (typeof window === "undefined") return { ...defaultDiscoverySettings };
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...defaultDiscoverySettings };
    const parsed = JSON.parse(raw) as Partial<DiscoverySettings>;
    return { ...defaultDiscoverySettings, ...parsed };
  } catch {
    return { ...defaultDiscoverySettings };
  }
}

export function saveDiscoverySettings(settings: DiscoverySettings) {
  if (typeof window === "undefined") return;
  window.localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
}

export function loadTargetProfiles(): TargetProfile[] {
  if (typeof window === "undefined") return [];
  try {
    const raw = window.localStorage.getItem(TARGET_PROFILES_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as TargetProfile[];
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((p) => p && typeof p.id === "string" && typeof p.name === "string" && typeof p.targets === "string");
  } catch {
    return [];
  }
}

export function saveTargetProfiles(profiles: TargetProfile[]) {
  if (typeof window === "undefined") return;
  window.localStorage.setItem(TARGET_PROFILES_KEY, JSON.stringify(profiles));
}

export function loadKnownDevices(): KnownDevice[] {
  if (typeof window === "undefined") return [];
  try {
    const raw = window.localStorage.getItem(KNOWN_DEVICES_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as KnownDevice[];
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(
      (d) =>
        d &&
        typeof d.ip === "string" &&
        d.ip.length > 0 &&
        (d.mac === undefined || typeof d.mac === "string") &&
        typeof d.lastSeenAt === "number",
    );
  } catch {
    return [];
  }
}

export function saveKnownDevices(devices: KnownDevice[]) {
  if (typeof window === "undefined") return;
  window.localStorage.setItem(KNOWN_DEVICES_KEY, JSON.stringify(devices));
}

export function clearKnownDevices() {
  if (typeof window === "undefined") return;
  window.localStorage.removeItem(KNOWN_DEVICES_KEY);
}
