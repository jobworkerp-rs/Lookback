import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { SidecarStatus } from "@/hooks/useSidecarStatus";
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

function renderDialog(onStarted = vi.fn()) {
  return renderWithProviders(
    <ImportDialog
      open
      onClose={vi.fn()}
      onStarted={onStarted}
      memoriesImportBin="/bin/memories-import"
      resolveError={null}
      sidecar={READY}
    />,
  );
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
