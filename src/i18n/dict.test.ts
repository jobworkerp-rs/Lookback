import { describe, expect, it } from "vitest";
import en from "./locales/en.json";
import ja from "./locales/ja.json";

type Dict = Record<string, unknown>;

/** Recursively flatten a nested dictionary into dotted leaf keys. */
function flatten(obj: Dict, prefix = ""): Map<string, string> {
  const out = new Map<string, string>();
  for (const [key, value] of Object.entries(obj)) {
    const full = prefix ? `${prefix}.${key}` : key;
    if (value && typeof value === "object" && !Array.isArray(value)) {
      for (const [k, v] of flatten(value as Dict, full)) out.set(k, v);
    } else {
      out.set(full, String(value));
    }
  }
  return out;
}

const PLURAL_SUFFIX = /_(zero|one|two|few|many|other)$/;

/** Normalise plural variants to their base key. */
function baseKeys(flat: Map<string, string>): Set<string> {
  const out = new Set<string>();
  for (const key of flat.keys()) out.add(key.replace(PLURAL_SUFFIX, ""));
  return out;
}

const flatJa = flatten(ja as Dict);
const flatEn = flatten(en as Dict);

// TEST-I18N-3 — AC-I18N-7
describe("dictionary key parity", () => {
  it("ja.json and en.json share the same base-key set", () => {
    const jaKeys = baseKeys(flatJa);
    const enKeys = baseKeys(flatEn);
    const missingInJa = [...enKeys].filter((k) => !jaKeys.has(k));
    const missingInEn = [...jaKeys].filter((k) => !enKeys.has(k));
    expect({ missingInJa, missingInEn }).toEqual({ missingInJa: [], missingInEn: [] });
  });
});

// Interpolation keys stay aligned across locales.
describe("interpolation placeholder parity", () => {
  it("matching keys carry the same set of {{placeholders}} in both languages", () => {
    const placeholders = (s: string) =>
      new Set([...s.matchAll(/\{\{\s*([a-zA-Z0-9_]+)[^}]*\}\}/g)].map((m) => m[1]));
    const mismatches: string[] = [];
    for (const [key, jaVal] of flatJa) {
      const enVal = flatEn.get(key);
      if (enVal === undefined) continue;
      const jaSet = placeholders(jaVal);
      const enSet = placeholders(enVal);
      const same = jaSet.size === enSet.size && [...jaSet].every((p) => enSet.has(p as string));
      if (!same) mismatches.push(key);
    }
    expect(mismatches).toEqual([]);
  });
});
