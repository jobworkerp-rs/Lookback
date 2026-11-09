import { waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { defaultSnapshot } from "@/hooks/useImportProgress";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { App } from "./App";

const { useImportProgressMock } = vi.hoisted(() => ({
  useImportProgressMock: vi.fn(),
}));

vi.mock("@/hooks/useImportProgress", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/hooks/useImportProgress")>();
  return { ...actual, useImportProgress: useImportProgressMock };
});

vi.mock("@/hooks/useSidecarStatus", () => ({
  isVectorDegraded: () => null,
  useSidecarStatus: () => ({ phase: "ready", warnings: [] }),
}));

vi.mock("@/hooks/useTheme", () => ({ useTheme: () => "light" }));
vi.mock("@/hooks/useLocale", () => ({ useLocale: () => ({ resolved: "ja" }) }));
vi.mock("@/hooks/useSettingsDirty", () => ({
  useSettingsDirty: () => ({ dirty: false, setDirty: vi.fn() }),
}));
vi.mock("@/hooks/useGeneratedRefresh", () => ({ useGeneratedRefresh: vi.fn() }));
vi.mock("@/hooks/useImportRefresh", () => ({ useImportRefresh: vi.fn() }));
vi.mock("@/hooks/useRagChat", () => ({ useRagChat: () => ({}) }));
vi.mock("@/hooks/useReflectionProgress", () => ({ useReflectionProgress: () => ({}) }));
vi.mock("@/hooks/useStepStreamProgress", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/hooks/useStepStreamProgress")>();
  return {
    ...actual,
    usePersonalityProgress: () => ({}),
    useSummaryProgress: () => ({}),
  };
});
vi.mock("@/hooks/useStagedProgress", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/hooks/useStagedProgress")>();
  return { ...actual, useSummaryGenerationProgress: () => ({}) };
});

vi.mock("@/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api")>();
  return {
    ...actual,
    getAppCapabilities: vi.fn().mockResolvedValue({
      experimental_issues_enabled: false,
      issue_tracking_configurable: false,
    }),
    getAppSettings: vi.fn().mockResolvedValue({ effective_timezone: "UTC" }),
    getConnectionConfig: vi
      .fn()
      .mockImplementation(
        async () => (await import("@/lib/connectionConfig")).LOCAL_CONNECTION_CONFIG,
      ),
    getSetupStatus: vi.fn().mockResolvedValue({ required: false, resume_apply: false }),
    setOutputLanguage: vi.fn().mockResolvedValue(undefined),
  };
});

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn().mockResolvedValue("") }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn().mockResolvedValue(vi.fn()) }));
vi.mock("@/components/Sidebar", () => ({ Sidebar: () => null }));
vi.mock("@/components/ImportDialog", () => ({ ImportDialog: () => null }));
vi.mock("@/components/EmbeddingSpaceStatus", () => ({ EmbeddingSpaceStatus: () => null }));
vi.mock("@/pages/Threads", async () => {
  const { ImportProgressPanel } = await import("@/components/ImportProgressPanel");
  return {
    Threads: ({
      importSnapshot,
      onClearImportProgress,
    }: {
      importSnapshot: ReturnType<typeof defaultSnapshot> | null;
      onClearImportProgress: () => void;
    }) =>
      importSnapshot ? (
        <ImportProgressPanel snapshot={importSnapshot} onClose={onClearImportProgress} />
      ) : null,
  };
});

describe("App import progress placement", () => {
  beforeEach(() => {
    void i18n.changeLanguage("ja");
    useImportProgressMock.mockReturnValue({
      snapshot: defaultSnapshot("job-app-import"),
      busy: true,
      reset: vi.fn(),
      clear: vi.fn(),
      cancel: vi.fn().mockResolvedValue(undefined),
      dismissToast: vi.fn(),
      toastVisible: true,
    });
  });

  it("renders import progress in the Threads panel without a toast", async () => {
    const { container } = renderWithProviders(<App />);

    await waitFor(() => {
      expect(container.querySelector(".import-progress-panel strong")).toHaveTextContent(
        "Importing sessions",
      );
    });
    expect(container.querySelector(".import-progress-panel")).toBeInTheDocument();
    expect(container.querySelector(".toast")).not.toBeInTheDocument();
  });
});
