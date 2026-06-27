import { fireEvent, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useLocale } from "@/hooks/useLocale";
import i18n from "@/i18n";
import { LOCALE_STORAGE_KEY } from "@/lib/locale";
import { renderWithProviders } from "@/test-utils";
import { Sidebar } from "./Sidebar";

// A minimal tree wiring the real useLocale into the Sidebar so the language
// segment exercises the full path (setPref → changeLanguage → <html lang>).
function Harness() {
  const locale = useLocale();
  return (
    <Sidebar
      current="threads"
      onChange={vi.fn()}
      threadCount={3}
      summaryCount={2}
      sidecar={{
        phase: "ready",
        warnings: [],
        endpoints: {
          jobworkerp_port: 9000,
          memories_port: 9010,
          conductor_port: 9020,
          mcp_server_port: null,
        },
      }}
      theme={{ pref: "system", setPref: vi.fn() }}
      locale={locale}
    />
  );
}

beforeEach(() => {
  localStorage.setItem(LOCALE_STORAGE_KEY, "ja");
  i18n.changeLanguage("ja");
});

afterEach(() => {
  localStorage.clear();
  document.documentElement.removeAttribute("lang");
});

// TEST-I18N-6 — AC-I18N-3 / AC-I18N-9
describe("Sidebar language switch", () => {
  it("switches UI shell strings ja → en and updates <html lang>", () => {
    renderWithProviders(<Harness />);

    // Starts in Japanese.
    expect(screen.getByText("スレッド")).toBeTruthy();
    expect(screen.getByText("定期実行")).toBeTruthy();

    // The language segment renders the same labels in both languages
    // ("日本語" / "English"); clicking "English" flips the UI.
    fireEvent.click(screen.getByRole("button", { name: "English" }));

    expect(screen.getByText("Threads")).toBeTruthy();
    expect(screen.getByText("Periodic Tasks")).toBeTruthy();
    expect(screen.queryByText("スレッド")).toBeNull();
    expect(document.documentElement.lang).toBe("en");
  });
});
