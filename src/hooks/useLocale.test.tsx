import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { LOCALE_STORAGE_KEY, loadLocalePref, resolveLocale } from "@/lib/locale";
import { useLocale } from "./useLocale";

afterEach(() => {
  localStorage.clear();
  document.documentElement.removeAttribute("lang");
  vi.unstubAllGlobals();
});

function stubNavigatorLanguage(lang: string) {
  vi.stubGlobal("navigator", { ...navigator, language: lang });
}

// Locale hook switching and document sync.
describe("useLocale", () => {
  it("switches i18n language and <html lang> when an explicit pref is set", () => {
    const { result } = renderHook(() => useLocale());
    act(() => result.current.setPref("en"));
    expect(i18n.language).toBe("en");
    expect(document.documentElement.lang).toBe("en");

    act(() => result.current.setPref("ja"));
    expect(i18n.language).toBe("ja");
    expect(document.documentElement.lang).toBe("ja");
  });

  it("resolves system pref against navigator.language", () => {
    stubNavigatorLanguage("ja-JP");
    const { result } = renderHook(() => useLocale());
    act(() => result.current.setPref("system"));
    expect(i18n.language).toBe("ja");
    expect(document.documentElement.lang).toBe("ja");
    expect(result.current.resolved).toBe("ja");
  });

  it("updates the resolved language when the system language changes", () => {
    stubNavigatorLanguage("ja-JP");
    const { result } = renderHook(() => useLocale());
    act(() => result.current.setPref("system"));
    expect(result.current.resolved).toBe("ja");

    stubNavigatorLanguage("en-US");
    act(() => window.dispatchEvent(new Event("languagechange")));

    expect(i18n.language).toBe("en");
    expect(document.documentElement.lang).toBe("en");
    expect(result.current.resolved).toBe("en");
  });

  it("persists the chosen preference", () => {
    const { result } = renderHook(() => useLocale());
    act(() => result.current.setPref("en"));
    expect(localStorage.getItem(LOCALE_STORAGE_KEY)).toBe("en");
  });
});

// TEST-I18N-7 — AC-I18N-9: the canonical resolve logic for <html lang> lives in
// lib/locale.ts (resolveLocale + loadLocalePref); index.html's inline script is a
// minimal mirror of the same rule.
describe("<html lang> initial determination (canonical logic)", () => {
  it("maps stored preference + system language to the effective lang", () => {
    localStorage.setItem(LOCALE_STORAGE_KEY, "system");
    expect(resolveLocale(loadLocalePref(), "ja-JP")).toBe("ja");
    expect(resolveLocale(loadLocalePref(), "en-US")).toBe("en");

    localStorage.setItem(LOCALE_STORAGE_KEY, "en");
    expect(resolveLocale(loadLocalePref(), "ja-JP")).toBe("en");
  });
});
