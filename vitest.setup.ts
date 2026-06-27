import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

// Some Node/Vitest combinations do not expose jsdom's storage on globalThis.
// Theme tests exercise storage directly, so install a small spec-compatible
// in-memory implementation without touching Node's experimental accessor.
const storageValues = new Map<string, string>();
Object.defineProperty(globalThis, "localStorage", {
  configurable: true,
  value: {
    get length() {
      return storageValues.size;
    },
    clear() {
      storageValues.clear();
    },
    getItem(key: string) {
      return storageValues.get(key) ?? null;
    },
    key(index: number) {
      return Array.from(storageValues.keys())[index] ?? null;
    },
    removeItem(key: string) {
      storageValues.delete(key);
    },
    setItem(key: string, value: string) {
      storageValues.set(key, String(value));
    },
  } satisfies Storage,
});

// vitest's `globals` is off, so testing-library's auto-cleanup (which keys off
// the global afterEach) never registers. Unmount rendered trees explicitly so
// repeated render() calls in a file don't leave duplicate DOM behind.
afterEach(() => {
  cleanup();
});
