import { afterEach, describe, expect, it } from "vitest";
import { LOCALE_STORAGE_KEY, loadLocalePref, resolveLocale, saveLocalePref } from "./locale";

afterEach(() => {
  localStorage.clear();
});

// Locale resolution.
describe("resolveLocale", () => {
  it("follows the system language when pref is system", () => {
    expect(resolveLocale("system", "ja-JP")).toBe("ja");
    expect(resolveLocale("system", "en-US")).toBe("en");
    expect(resolveLocale("system", "fr-FR")).toBe("en");
  });

  it("ignores the system language when pref is explicit", () => {
    expect(resolveLocale("ja", "en-US")).toBe("ja");
    expect(resolveLocale("en", "ja-JP")).toBe("en");
  });

  it("handles case and empty boundary inputs", () => {
    expect(resolveLocale("system", "JA")).toBe("ja");
    expect(resolveLocale("system", "Ja-jp")).toBe("ja");
    expect(resolveLocale("system", "")).toBe("en");
  });
});

// Locale persistence.
describe("loadLocalePref / saveLocalePref", () => {
  it("defaults to system when unset", () => {
    expect(loadLocalePref()).toBe("system");
  });

  it("defaults to system for a malformed stored value", () => {
    localStorage.setItem(LOCALE_STORAGE_KEY, "klingon");
    expect(loadLocalePref()).toBe("system");
  });

  it("round-trips a saved preference", () => {
    saveLocalePref("ja");
    expect(loadLocalePref()).toBe("ja");
    saveLocalePref("en");
    expect(loadLocalePref()).toBe("en");
  });

  it("falls back to system when localStorage throws", () => {
    const original = globalThis.localStorage;
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      get() {
        throw new Error("storage unavailable");
      },
    });
    try {
      expect(loadLocalePref()).toBe("system");
    } finally {
      Object.defineProperty(globalThis, "localStorage", {
        configurable: true,
        value: original,
      });
    }
  });
});
