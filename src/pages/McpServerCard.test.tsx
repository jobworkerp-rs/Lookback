import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactElement } from "react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import type { McpSettingsResponse, SetMcpSettingsRequest } from "@/types/api";
import { McpServerCard } from "./Settings";

const getMcpSettings = vi.fn();

vi.mock("@/api", () => ({
  getMcpSettings: () => getMcpSettings(),
  // McpServerCard does not call these, but Settings.tsx re-exports them in the
  // same module — stub so the mocked module is complete.
  applySettings: () => Promise.resolve({ restarted: true, backup_path: null }),
  createDataRoot: () => Promise.resolve(),
  getAppSettings: () => Promise.resolve(null),
  getConnectionConfig: () => Promise.resolve(null),
  getEmbeddingSettings: () => Promise.resolve(null),
  getLlmSettings: () => Promise.resolve(null),
  getMemoryEmbeddingStats: () => Promise.resolve(null),
  getModelStatus: () => Promise.resolve({ llm: null, embedding: null }),
  getSettings: () => Promise.resolve(null),
  listEmbeddingPresets: () => Promise.resolve([]),
  listLlmPresets: () => Promise.resolve([]),
  purgeAllData: () => Promise.resolve({ warnings: [] }),
  readSidecarLog: () =>
    Promise.resolve({ file_name: "", content: "", truncated: false, file_size: 0 }),
  redispatchMemoryEmbeddings: () => Promise.resolve(null),
  retryModelSetup: () => Promise.resolve(),
  setConnectionConfig: () => Promise.resolve(),
  setDataRoot: () => Promise.resolve(),
  validateDataRoot: () => Promise.resolve(null),
}));

const DISABLED_LOCAL: McpSettingsResponse = {
  enabled: false,
  exclude_runner_as_tool: null,
  exclude_worker_as_tool: null,
  streaming: null,
  request_timeout_sec: null,
  set_name: "lookback-mcp-rag",
  active_port: null,
};

function renderCard(onDirtyChange = vi.fn()) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const ui: ReactElement = <McpServerCard onDirtyChange={onDirtyChange} resetSignal={0} />;
  render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>{ui}</QueryClientProvider>
    </I18nextProvider>,
  );
  return { onDirtyChange };
}

/** Latest payload reported via onDirtyChange. */
function lastPayload(onDirty: ReturnType<typeof vi.fn>): SetMcpSettingsRequest | null {
  return onDirty.mock.lastCall?.[0] ?? null;
}

/** Latest edited flag reported via onDirtyChange. */
function lastEdited(onDirty: ReturnType<typeof vi.fn>): boolean {
  return onDirty.mock.lastCall?.[1] ?? false;
}

describe("McpServerCard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    getMcpSettings.mockReset();
  });

  it("reports a payload and edited=true when the toggle is flipped on", async () => {
    getMcpSettings.mockResolvedValue(DISABLED_LOCAL);
    const { onDirtyChange: onDirty } = renderCard();
    await waitFor(() => expect(getMcpSettings).toHaveBeenCalled());
    // Initially clean (matches persisted disabled state).
    await waitFor(() => expect(lastPayload(onDirty)).toBeNull());

    const toggle = screen.getByRole("checkbox") as HTMLInputElement;
    fireEvent.click(toggle);
    await waitFor(() => {
      expect(lastEdited(onDirty)).toBe(true);
      expect(lastPayload(onDirty)).toMatchObject({ enabled: true });
    });
  });

  it("keeps the toggle enabled and saveable (MCP is not gated by connection mode)", async () => {
    // MCP runs in the local sidecar regardless of remote browse mode, so the
    // card must never be disabled — enabling it while remote is a valid action.
    getMcpSettings.mockResolvedValue(DISABLED_LOCAL);
    const { onDirtyChange: onDirty } = renderCard();
    await waitFor(() => expect(getMcpSettings).toHaveBeenCalled());
    const toggle = screen.getByRole("checkbox") as HTMLInputElement;
    expect(toggle.disabled).toBe(false);
    // Toggle inside waitFor: the query resolves asynchronously, so a single
    // pre-load click would build a null payload (buildPayload bails on
    // `!data`). Re-clicking until the effect reports the enabled payload
    // tolerates the load race without a brittle fixed delay.
    await waitFor(() => {
      if (!toggle.checked) fireEvent.click(toggle);
      expect(lastPayload(onDirty)).toMatchObject({ enabled: true });
    });
  });

  it("shows the connection URL and client config when enabled with an active port", async () => {
    getMcpSettings.mockResolvedValue({ ...DISABLED_LOCAL, enabled: true, active_port: 39010 });
    renderCard();
    await waitFor(() => expect(screen.getByText("http://127.0.0.1:39010/mcp")).toBeInTheDocument());
    expect(screen.getByText(/"url": "http:\/\/127.0.0.1:39010\/mcp"/)).toBeInTheDocument();
  });

  it("prompts to save when enabled but no port is bound yet", async () => {
    getMcpSettings.mockResolvedValue({ ...DISABLED_LOCAL, enabled: true, active_port: null });
    renderCard();
    await waitFor(() =>
      expect(screen.getByText(/保存して sidecar が再起動すると表示されます/)).toBeInTheDocument(),
    );
  });

  it("rejects an out-of-range timeout (mirrors the backend [1,3600] bound)", async () => {
    // The backend's validate_set_request rejects >3600, so the card must NOT
    // produce a saveable payload for it — otherwise the save bar arms for a
    // restart the backend then fails (the very thing the gate exists to avoid).
    getMcpSettings.mockResolvedValue({ ...DISABLED_LOCAL, enabled: true, active_port: 39010 });
    const { onDirtyChange: onDirty } = renderCard();
    await waitFor(() => expect(getMcpSettings).toHaveBeenCalled());
    // Reveal the advanced section that holds the timeout input.
    fireEvent.click(screen.getByRole("button", { name: /上級設定/ }));
    const timeout = await screen.findByPlaceholderText("60");

    // Above the max ⇒ invalid ⇒ null payload + error hint.
    fireEvent.change(timeout, { target: { value: "3601" } });
    await waitFor(() => {
      expect(screen.getByText(/秒の整数で入力してください/)).toBeInTheDocument();
      expect(lastPayload(onDirty)).toBeNull();
    });

    // In range ⇒ saveable payload carries the value.
    fireEvent.change(timeout, { target: { value: "120" } });
    await waitFor(() =>
      expect(lastPayload(onDirty)).toMatchObject({ enabled: true, request_timeout_sec: 120 }),
    );
  });

  it("labels the streaming default as 無効 to match mcp-server's MCP_STREAMING default (false)", async () => {
    // Regression: the default option used to read 既定 (有効), but an empty
    // value omits MCP_STREAMING on save and mcp-server then falls back to its
    // own default of `false`. The label MUST match that real behaviour so a
    // user who saves with the default does not silently get streaming OFF
    // while the UI claims ON. (`mcp-server/src/config.rs`: streaming = false.)
    getMcpSettings.mockResolvedValue({ ...DISABLED_LOCAL, enabled: true, active_port: 39010 });
    renderCard();
    await waitFor(() => expect(getMcpSettings).toHaveBeenCalled());
    fireEvent.click(screen.getByRole("button", { name: /上級設定/ }));
    const streamingDefault = await screen.findByRole("option", { name: /既定/ });
    expect(streamingDefault).toHaveTextContent("既定 (無効)");
  });

  it("omits streaming from the payload when left at the default (server applies false)", async () => {
    // The empty value must NOT serialise a streaming field — that is what lets
    // mcp-server apply its own default instead of pinning a literal.
    getMcpSettings.mockResolvedValue({ ...DISABLED_LOCAL, enabled: true, active_port: 39010 });
    const { onDirtyChange: onDirty } = renderCard();
    await waitFor(() => expect(getMcpSettings).toHaveBeenCalled());
    fireEvent.click(screen.getByRole("button", { name: /上級設定/ }));
    // Make an unrelated edit so a payload is produced, then assert streaming
    // is absent (default left untouched).
    const timeout = await screen.findByPlaceholderText("60");
    fireEvent.change(timeout, { target: { value: "120" } });
    await waitFor(() => {
      const payload = lastPayload(onDirty);
      expect(payload).toMatchObject({ enabled: true, request_timeout_sec: 120 });
      expect(payload?.streaming ?? null).toBeNull();
    });
  });

  it("reports edited=true for an invalid timeout even though the payload is null", async () => {
    // Regression: an out-of-range timeout collapses timeoutVal to null, which
    // used to match a persisted-null and report edited=false — the leave-guard
    // then let the bad input be discarded without a confirm. The raw input
    // differs from the persisted value, so edited MUST stay true.
    getMcpSettings.mockResolvedValue({ ...DISABLED_LOCAL, enabled: true, active_port: 39010 });
    const { onDirtyChange: onDirty } = renderCard();
    await waitFor(() => expect(getMcpSettings).toHaveBeenCalled());
    fireEvent.click(screen.getByRole("button", { name: /上級設定/ }));
    const timeout = await screen.findByPlaceholderText("60");

    fireEvent.change(timeout, { target: { value: "3601" } });
    await waitFor(() => {
      expect(lastPayload(onDirty)).toBeNull(); // not saveable
      expect(lastEdited(onDirty)).toBe(true); // but still guards navigation
    });
  });
});
