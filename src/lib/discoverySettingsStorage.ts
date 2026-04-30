import { defaultDiscoverySettings, type DiscoverySettings } from "$lib/discoveryDefaults";

const STORAGE_KEY = "tm.discovery.settings.v1";
const TARGET_PROFILES_KEY = "tm.discovery.targetProfiles.v1";

export type TargetProfile = {
  id: string;
  name: string;
  targets: string;
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
