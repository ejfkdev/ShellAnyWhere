/// Persistent token storage using localStorage.

const STORAGE_KEY = "shell-anywhere";

export interface StoredConfig {
  serverToken: string;
  agentToken: string;
  fontSize: number;
}

const defaults: StoredConfig = {
  serverToken: "",
  agentToken: "",
  fontSize: 14,
};

export function loadConfig(): StoredConfig {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...defaults };
    return { ...defaults, ...JSON.parse(raw) };
  } catch {
    return { ...defaults };
  }
}

export function saveConfig(cfg: Partial<StoredConfig>) {
  const current = loadConfig();
  const merged = { ...current, ...cfg };
  localStorage.setItem(STORAGE_KEY, JSON.stringify(merged));
}

// ── Tab order persistence ──

const TAB_ORDER_KEY = "shell-anywhere-tab-order";

export function loadTabOrder(): string[] {
  try {
    const raw = localStorage.getItem(TAB_ORDER_KEY);
    return raw ? JSON.parse(raw) : [];
  } catch {
    return [];
  }
}

export function saveTabOrder(ids: string[]) {
  localStorage.setItem(TAB_ORDER_KEY, JSON.stringify(ids));
}
