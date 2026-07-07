import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { SidecarStatus } from "@/hooks/useSidecarStatus";
import { TimezoneContext } from "@/hooks/useTimezone";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { ImportDialog } from "./ImportDialog";

const startImport = vi.fn();
const openDialog = vi.fn();

vi.mock("@/api", () => ({
  startImport: (req: unknown) => startImport(req),
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (opts: unknown) => openDialog(opts),
}));

const READY: SidecarStatus = { phase: "ready", warnings: [] };

function importDialog(open = true, onStarted = vi.fn()) {
  return (
    <ImportDialog
      open={open}
      onClose={vi.fn()}
      onStarted={onStarted}
      memoriesImportBin="/bin/memories-import"
      resolveError={null}
      sidecar={READY}
    />
  );
}

function renderDialog(onStarted = vi.fn()) {
  return renderWithProviders(importDialog(true, onStarted));
}

function renderDialogWithTimezone(timezone: string, open = true) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const renderTree = (isOpen: boolean) => (
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <TimezoneContext.Provider value={timezone}>{importDialog(isOpen)}</TimezoneContext.Provider>
      </QueryClientProvider>
    </I18nextProvider>
  );
  const result = render(renderTree(open));
  return {
    ...result,
    rerenderDialog: (isOpen: boolean) => result.rerender(renderTree(isOpen)),
  };
}

/** Enable the plain source checkbox so its sub-block renders. */
function enablePlain() {
  fireEvent.click(screen.getByLabelText(i18n.t("import.targetPlain")));
}

beforeEach(() => {
  startImport.mockReset();
  openDialog.mockReset();
  localStorage.clear();
  startImport.mockResolvedValue({ job_id: "import-1" });
  i18n.changeLanguage("ja");
});

afterEach(() => {
  vi.unstubAllEnvs();
  vi.useRealTimers();
});

describe("ImportDialog since date timezone", () => {
  it("seeds the default since date from the display timezone when the dialog opens", async () => {
    vi.stubEnv("TZ", "Asia/Tokyo");
    vi.useFakeTimers({ now: new Date("2026-01-02T03:00:00.000Z"), toFake: ["Date"] });

    const { rerenderDialog } = renderDialogWithTimezone("America/New_York", false);

    rerenderDialog(true);
    fireEvent.click(screen.getByText(i18n.t("import.start")));

    expect(startImport).toHaveBeenCalled();
    const req = startImport.mock.calls[0]?.[0];
    expect(req.since).toBe("2025-12-31T05:00:00.000Z");
  });

  it("keeps a user-edited since date when the dialog closes and reopens", async () => {
    vi.stubEnv("TZ", "Asia/Tokyo");
    vi.useFakeTimers({ now: new Date("2026-01-02T03:00:00.000Z"), toFake: ["Date"] });

    const { rerenderDialog } = renderDialogWithTimezone("America/New_York");
    await act(async () => {});
    fireEvent.change(screen.getByDisplayValue("2025-12-31"), {
      target: { value: "2025-12-25" },
    });
    expect(screen.getByDisplayValue("2025-12-25")).toBeTruthy();
    await act(async () => {});
    rerenderDialog(false);
    rerenderDialog(true);
    expect(screen.getByDisplayValue("2025-12-25")).toBeTruthy();
    fireEvent.click(screen.getByText(i18n.t("import.start")));

    expect(startImport).toHaveBeenCalled();
    const req = startImport.mock.calls[0]?.[0];
    expect(req.since).toBe("2025-12-25T05:00:00.000Z");
  });
});

describe("ImportDialog plain source", () => {
  it("reveals the plain sub-block only when the plain checkbox is on", () => {
    renderDialog();
    expect(screen.queryByText(i18n.t("import.plainStrategy"))).toBeNull();
    enablePlain();
    expect(screen.getByText(i18n.t("import.plainStrategy"))).toBeTruthy();
  });

  it("fills the root from the directory picker", async () => {
    openDialog.mockResolvedValue("/Users/me/notes");
    renderDialog();
    enablePlain();
    fireEvent.click(screen.getByText(i18n.t("import.plainChoose")));
    await waitFor(() => expect(screen.getByText("/Users/me/notes")).toBeTruthy());
    expect(openDialog).toHaveBeenCalledWith({ directory: true, multiple: false });
  });

  it("sends sources with plain and a plain config on submit", async () => {
    openDialog.mockResolvedValue("/Users/me/notes");
    renderDialog();
    // Drop the auto-on claude/codex so the request isolates the plain path.
    fireEvent.click(screen.getByLabelText(/claude-code/));
    fireEvent.click(screen.getByLabelText(/codex/));
    enablePlain();
    fireEvent.click(screen.getByText(i18n.t("import.plainChoose")));
    await waitFor(() => screen.getByText("/Users/me/notes"));
    fireEvent.change(screen.getByPlaceholderText("obsidian-private"), {
      target: { value: "notes" },
    });
    fireEvent.click(screen.getByText(i18n.t("import.start")));

    await waitFor(() => expect(startImport).toHaveBeenCalled());
    const req = startImport.mock.calls[0]?.[0];
    expect(req.sources).toEqual(["plain"]);
    expect(req.plain).toMatchObject({
      root: "/Users/me/notes",
      source_name: "notes",
      thread_strategy: "per-dir",
    });
  });

  it("blocks submit with an empty root", async () => {
    renderDialog();
    enablePlain();
    fireEvent.click(screen.getByText(i18n.t("import.start")));
    await waitFor(() => expect(screen.getByText(i18n.t("import.errorNoPlainRoot"))).toBeTruthy());
    expect(startImport).not.toHaveBeenCalled();
  });

  it("blocks submit with an invalid source name", async () => {
    openDialog.mockResolvedValue("/Users/me/notes");
    renderDialog();
    enablePlain();
    fireEvent.click(screen.getByText(i18n.t("import.plainChoose")));
    await waitFor(() => screen.getByText("/Users/me/notes"));
    fireEvent.change(screen.getByPlaceholderText("obsidian-private"), {
      target: { value: "Bad Name" },
    });
    fireEvent.click(screen.getByText(i18n.t("import.start")));
    await waitFor(() =>
      expect(screen.getByText(i18n.t("import.errorInvalidSourceName"))).toBeTruthy(),
    );
    expect(startImport).not.toHaveBeenCalled();
  });

  it("persists the chosen strategy across mounts", async () => {
    openDialog.mockResolvedValue("/Users/me/notes");
    const { unmount } = renderDialog();
    enablePlain();
    fireEvent.change(screen.getByDisplayValue(i18n.t("import.plainStrategyPerDir")), {
      target: { value: "single" },
    });
    unmount();
    renderDialog();
    enablePlain();
    expect(
      (screen.getByDisplayValue(i18n.t("import.plainStrategySingle")) as HTMLSelectElement).value,
    ).toBe("single");
  });
});
