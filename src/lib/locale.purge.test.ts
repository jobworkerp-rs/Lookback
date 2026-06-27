import { afterEach, describe, expect, it, vi } from "vitest";

// purge_all_data is a Rust-side Tauri command that deletes the data root but never
// touches the browser's localStorage, where the locale preference lives. This test
// guards AC-I18N-6 structurally: (a) the value survives a purgeAllData() call, and
// (b) lib/locale.ts never reaches Tauri (invoke), so it cannot be coupled to any
// purge-style command — persistence is localStorage-only.

const invoke = vi.fn(async () => ({ data_root_deleted: true }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

afterEach(() => {
  localStorage.clear();
  invoke.mockClear();
});

// TEST-I18N-8 — AC-I18N-6
describe("locale preference survives purge_all_data", () => {
  it("keeps lookback.locale in localStorage after purgeAllData()", async () => {
    const { LOCALE_STORAGE_KEY } = await import("./locale");
    const { purgeAllData } = await import("@/api");
    localStorage.setItem(LOCALE_STORAGE_KEY, "en");

    await purgeAllData();

    expect(localStorage.getItem(LOCALE_STORAGE_KEY)).toBe("en");
  });

  it("lib/locale.ts persists without touching Tauri (invoke)", async () => {
    const locale = await import("./locale");
    locale.saveLocalePref("ja");
    locale.loadLocalePref();
    locale.resolveLocale("system", "ja-JP");
    expect(invoke).not.toHaveBeenCalled();
  });
});
