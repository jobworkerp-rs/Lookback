// Persistence for the plain-import thread-grouping choice. Like theme/locale,
// this is a pure UI preference: it lives only in localStorage and deliberately
// does NOT import `@tauri-apps/api`, so "delete all data" (purge_all_data)
// never touches it.

import type { ThreadStrategy } from "@/types/api";

export const PLAIN_STRATEGY_STORAGE_KEY = "lookback.plainThreadStrategy";

/** Default grouping when nothing has been saved yet: one thread per directory. */
export const DEFAULT_PLAIN_STRATEGY: ThreadStrategy = "per-dir";

const STRATEGIES = ["per-file", "per-dir", "single"] as const;

function isThreadStrategy(v: unknown): v is ThreadStrategy {
  return typeof v === "string" && (STRATEGIES as readonly string[]).includes(v);
}

/** Read the saved strategy; unset or malformed values fall back to the default. */
export function loadPlainThreadStrategy(): ThreadStrategy {
  try {
    const v = localStorage.getItem(PLAIN_STRATEGY_STORAGE_KEY);
    return isThreadStrategy(v) ? v : DEFAULT_PLAIN_STRATEGY;
  } catch {
    // Private mode / disabled storage — default rather than crash.
    return DEFAULT_PLAIN_STRATEGY;
  }
}

export function savePlainThreadStrategy(strategy: ThreadStrategy): void {
  try {
    localStorage.setItem(PLAIN_STRATEGY_STORAGE_KEY, strategy);
  } catch {
    // Best-effort; a failed write just means the choice isn't persisted.
  }
}

/** `^[a-z0-9_-]{1,32}$` — the charset memories' `--source-name` enforces. */
export function isValidPlainSourceName(name: string): boolean {
  return /^[a-z0-9_-]{1,32}$/.test(name);
}
