import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { LOCAL_CONNECTION_CONFIG } from "@/lib/connectionConfig";
import { remoteConnectionConfig } from "@/test-utils";
import type { RemoteJobworkerpReadiness } from "@/types/api";
import { ConnectionCard } from "./ConnectionCard";

const getConnectionConfig = vi.fn();
const setConnectionConfig = vi.fn();
const testConnectionConfig = vi.fn();
const getIssueTrackerSettings = vi.fn();
const inspectRemoteJobworkerp = vi.fn();
const registerRemoteJobworkerpWorkers = vi.fn();
vi.mock("@/api", () => ({
  getConnectionConfig: () => getConnectionConfig(),
  setConnectionConfig: (cfg: unknown) => setConnectionConfig(cfg),
  testConnectionConfig: (cfg: unknown) => testConnectionConfig(cfg),
  getIssueTrackerSettings: () => getIssueTrackerSettings(),
  inspectRemoteJobworkerp: () => inspectRemoteJobworkerp(),
  registerRemoteJobworkerpWorkers: () => registerRemoteJobworkerpWorkers(),
}));

const remote = (url: string) => ({ mode: "remote" as const, remote_url: url });

const REMOTE_JOBWORKERP = remoteConnectionConfig("http://j:9000", "http://m:9010");

const UNPREPARED: RemoteJobworkerpReadiness = {
  jobworkerp_url: "http://j:9000",
  missing_workers: ["thread-summary"],
  missing_function_sets: [],
  registration_outdated: true,
  missing_channels: ["llm"],
  missing_runners: ["LLMPromptRunner"],
  environment: ["WORKER_CHANNELS=llm", "TZ=Asia/Tokyo"],
  ready: false,
};

function renderCard() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const invalidateSpy = vi.spyOn(client, "invalidateQueries");
  const onDirtyChange = vi.fn();
  const ui = (resetSignal: number) => (
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <ConnectionCard
          localJobworkerpUrl="http://127.0.0.1:9000"
          localMemoriesUrl="http://127.0.0.1:9010"
          onDirtyChange={onDirtyChange}
          resetSignal={resetSignal}
        />
      </QueryClientProvider>
    </I18nextProvider>
  );
  const { rerender } = render(ui(0));
  return { invalidateSpy, onDirtyChange, reset: () => rerender(ui(1)) };
}

/** The payload and edited flag the card reported last. */
const lastReport = (onDirtyChange: ReturnType<typeof vi.fn>) => onDirtyChange.mock.lastCall;

const service = (name: string) => screen.getByRole("group", { name });
const modeButton = (name: string, label: "Local sidecar" | "Remote server") =>
  within(service(name)).getByRole("button", { name: label });

async function chooseRemote(name: string, url: string) {
  fireEvent.click(modeButton(name, "Remote server"));
  fireEvent.change(within(service(name)).getByRole("textbox", { name: `${name} URL` }), {
    target: { value: url },
  });
}

describe("ConnectionCard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    for (const mock of [
      getConnectionConfig,
      setConnectionConfig,
      testConnectionConfig,
      getIssueTrackerSettings,
      inspectRemoteJobworkerp,
      registerRemoteJobworkerpWorkers,
    ]) {
      mock.mockReset();
    }
    getConnectionConfig.mockResolvedValue(LOCAL_CONNECTION_CONFIG);
    getIssueTrackerSettings.mockResolvedValue({ enabled: false, effective_enabled: false });
  });

  it("shows the live local URLs read-only", async () => {
    renderCard();
    await screen.findByDisplayValue("http://127.0.0.1:9010");
    expect(screen.getByDisplayValue("http://127.0.0.1:9000")).toHaveAttribute("readonly");
    expect(within(service("memories")).queryByRole("textbox", { name: "memories URL" })).toBeNull();
  });

  it("reports the edited targets for the save bar instead of saving them", async () => {
    const { onDirtyChange } = renderCard();
    await screen.findByDisplayValue("http://127.0.0.1:9010");
    expect(lastReport(onDirtyChange)).toEqual([null, false]);

    await chooseRemote("memories", " http://m:9010 ");

    expect(lastReport(onDirtyChange)).toEqual([
      { ...LOCAL_CONNECTION_CONFIG, memories: remote("http://m:9010") },
      true,
    ]);
    expect(setConnectionConfig).not.toHaveBeenCalled();
  });

  it("discards the edits on reset", async () => {
    const { onDirtyChange, reset } = renderCard();
    await screen.findByDisplayValue("http://127.0.0.1:9010");
    await chooseRemote("memories", "http://m:9010");

    reset();

    await waitFor(() => expect(lastReport(onDirtyChange)).toEqual([null, false]));
    expect(within(service("memories")).queryByRole("textbox", { name: "memories URL" })).toBeNull();
  });

  it("tests the edited targets without saving them", async () => {
    testConnectionConfig.mockResolvedValue({
      jobworkerp_url: "http://127.0.0.1:9000",
      memories_url: "http://m:9010",
    });
    const { invalidateSpy } = renderCard();
    await screen.findByDisplayValue("http://127.0.0.1:9010");

    await chooseRemote("memories", "http://m:9010");
    fireEvent.click(screen.getByText("接続をテスト"));

    await screen.findByText("接続できました");
    expect(testConnectionConfig).toHaveBeenCalledWith({
      ...LOCAL_CONNECTION_CONFIG,
      memories: remote("http://m:9010"),
    });
    expect(setConnectionConfig).not.toHaveBeenCalled();
    expect(invalidateSpy).not.toHaveBeenCalled();
  });

  it("offers a remote jobworkerp only after memories is remote, and conductor after jobworkerp", async () => {
    renderCard();
    await screen.findByDisplayValue("http://127.0.0.1:9010");
    expect(modeButton("jobworkerp", "Remote server")).toBeDisabled();
    expect(modeButton("conductor", "Remote server")).toBeDisabled();

    await chooseRemote("memories", "http://m:9010");
    expect(modeButton("jobworkerp", "Remote server")).toBeEnabled();
    await chooseRemote("jobworkerp", "http://j:9000");

    expect(modeButton("memories", "Local sidecar")).toBeDisabled();
    expect(within(service("memories")).getByText(/jobworkerp がリモートの間/)).toBeInTheDocument();
    expect(modeButton("conductor", "Remote server")).toBeEnabled();
  });

  it("shows the Issue tracker and its gateway URLs only while issue tracking is enabled", async () => {
    getIssueTrackerSettings.mockResolvedValue({ enabled: true, effective_enabled: true });
    const { onDirtyChange } = renderCard();
    await screen.findByRole("group", { name: "Issue tracker" });

    await chooseRemote("Issue tracker", "http://t:9030");
    const tracker = within(service("Issue tracker"));
    fireEvent.change(tracker.getByRole("textbox", { name: "gateway control URL" }), {
      target: { value: "http://t:9031" },
    });
    expect(lastReport(onDirtyChange)).toEqual([null, true]);
    fireEvent.change(tracker.getByRole("textbox", { name: "gateway worker URL" }), {
      target: { value: "http://t:9032" },
    });

    expect(lastReport(onDirtyChange)).toEqual([
      {
        ...LOCAL_CONNECTION_CONFIG,
        issue_tracker: {
          mode: "remote",
          remote_url: "http://t:9030",
          remote_gateway_control_url: "http://t:9031",
          remote_gateway_worker_url: "http://t:9032",
        },
      },
      true,
    ]);
  });

  it("hides the Issue tracker while issue tracking is disabled", async () => {
    renderCard();
    await screen.findByDisplayValue("http://127.0.0.1:9010");
    expect(screen.queryByRole("group", { name: "Issue tracker" })).toBeNull();
  });

  it("asks for jobworkerp host paths only while jobworkerp is remote", async () => {
    renderCard();
    await screen.findByDisplayValue("http://127.0.0.1:9010");
    expect(screen.queryByText("jobworkerp ホスト上のパス")).toBeNull();

    await chooseRemote("memories", "http://m:9010");
    await chooseRemote("jobworkerp", "http://j:9000");
    expect(screen.getByText("jobworkerp ホスト上のパス")).toBeInTheDocument();
  });

  it("does not inspect a local jobworkerp", async () => {
    renderCard();
    await screen.findByDisplayValue("http://127.0.0.1:9010");
    expect(inspectRemoteJobworkerp).not.toHaveBeenCalled();
    expect(screen.queryByText("リモート jobworkerp の準備状況")).toBeNull();
  });

  it("reports what the saved remote jobworkerp lacks and registers the workers", async () => {
    getConnectionConfig.mockResolvedValue(REMOTE_JOBWORKERP);
    inspectRemoteJobworkerp.mockResolvedValue(UNPREPARED);
    registerRemoteJobworkerpWorkers.mockResolvedValue({
      warnings: [],
      readiness: {
        ...UNPREPARED,
        missing_workers: [],
        registration_outdated: false,
      },
    });
    renderCard();

    await screen.findByText("thread-summary");
    expect(screen.getByText("llm")).toBeInTheDocument();
    expect(screen.getByText("LLMPromptRunner")).toBeInTheDocument();
    expect(screen.getByText(/別のバージョン/)).toBeInTheDocument();
    expect(screen.getByText(/WORKER_CHANNELS=llm/)).toBeInTheDocument();

    fireEvent.click(screen.getByText("worker を登録"));

    await waitFor(() => expect(screen.queryByText("thread-summary")).toBeNull());
    expect(registerRemoteJobworkerpWorkers).toHaveBeenCalledTimes(1);
    expect(screen.queryByText(/別のバージョン/)).toBeNull();
    // Channels and plugins are set on the remote host, not by registration.
    expect(screen.getByText("LLMPromptRunner")).toBeInTheDocument();
  });

  it("shows registration warnings", async () => {
    getConnectionConfig.mockResolvedValue(REMOTE_JOBWORKERP);
    inspectRemoteJobworkerp.mockResolvedValue(UNPREPARED);
    registerRemoteJobworkerpWorkers.mockResolvedValue({
      warnings: [{ kind: "worker_apply_failed", message: "apply failed: llm", detail: null }],
      readiness: UNPREPARED,
    });
    renderCard();
    await screen.findByText("thread-summary");

    fireEvent.click(screen.getByText("worker を登録"));

    await screen.findByText("apply failed: llm");
  });

  it("reports a ready remote jobworkerp", async () => {
    getConnectionConfig.mockResolvedValue(REMOTE_JOBWORKERP);
    inspectRemoteJobworkerp.mockResolvedValue({
      ...UNPREPARED,
      missing_workers: [],
      registration_outdated: false,
      missing_channels: [],
      missing_runners: [],
      ready: true,
    });
    renderCard();
    await screen.findByText("準備ができています");
  });
});
