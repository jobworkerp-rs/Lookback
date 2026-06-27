import { afterEach, describe, expect, it } from "vitest";
import { applyTheme, loadThemePref, resolveTheme, saveThemePref, THEME_STORAGE_KEY } from "./theme";

afterEach(() => {
  localStorage.clear();
  document.documentElement.removeAttribute("data-theme");
  document.documentElement.style.colorScheme = "";
});

describe("resolveTheme", () => {
  it("follows the OS setting when pref is system", () => {
    expect(resolveTheme("system", true)).toBe("dark");
    expect(resolveTheme("system", false)).toBe("light");
  });

  it("ignores the OS setting when pref is explicit", () => {
    expect(resolveTheme("dark", false)).toBe("dark");
    expect(resolveTheme("light", true)).toBe("light");
  });
});

describe("loadThemePref / saveThemePref", () => {
  it("defaults to system when unset", () => {
    expect(loadThemePref()).toBe("system");
  });

  it("defaults to system for a malformed stored value", () => {
    localStorage.setItem(THEME_STORAGE_KEY, "neon");
    expect(loadThemePref()).toBe("system");
  });

  it("round-trips a saved preference", () => {
    saveThemePref("dark");
    expect(loadThemePref()).toBe("dark");
    saveThemePref("light");
    expect(loadThemePref()).toBe("light");
  });
});

describe("applyTheme", () => {
  it("sets data-theme=dark and color-scheme for dark", () => {
    applyTheme("dark");
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
    expect(document.documentElement.style.colorScheme).toBe("dark");
  });

  it("removes data-theme for light", () => {
    document.documentElement.setAttribute("data-theme", "dark");
    applyTheme("light");
    expect(document.documentElement.hasAttribute("data-theme")).toBe(false);
    expect(document.documentElement.style.colorScheme).toBe("light");
  });
});
