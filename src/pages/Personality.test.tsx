import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactElement } from "react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { StepStreamProgressHandle } from "@/hooks/useStepStreamProgress";
import i18n from "@/i18n";
import { Personality } from "./Personality";

const enqueuePersonalityJob = vi.fn();
const enqueuePersonalityMergeJob = vi.fn();
const getPersonality = vi.fn();
const findMemoryThreadPosition = vi.fn();
// Stubs for the other api surface Personality.tsx imports. They aren't hit by
// the merge-button path, but the module-level import resolves at render time
// regardless, so all named exports the page imports must be present.
const listPersonalitySignals = vi.fn();
const deletePersonalityProfile = vi.fn();
const deletePersonalitySignal = vi.fn();

vi.mock("@/api", () => ({
  enqueuePersonalityJob: (req: unknown) => enqueuePersonalityJob(req),
  enqueuePersonalityMergeJob: (req: unknown) => enqueuePersonalityMergeJob(req),
  findMemoryThreadPosition: (req: unknown) => findMemoryThreadPosition(req),
  getPersonality: (req: unknown) => getPersonality(req),
  listPersonalitySignals: (req: unknown) => listPersonalitySignals(req),
  deletePersonalityProfile: (id: unknown) => deletePersonalityProfile(id),
  deletePersonalitySignal: (id: unknown) => deletePersonalitySignal(id),
  // Pass-throughs needed for module load — the merge tests don't exercise
  // these parsers, so identity stubs are enough.
  parsePersonalityContent: (s: { content_json: string }) => JSON.parse(s.content_json),
  parsePersonalitySignalContent: (s: unknown) => s,
}));

vi.mock("@/components/ThreadDetail", () => ({
  ThreadDetail: ({ thread }: { thread: { id: string } }) => <div>Thread #{thread.id}</div>,
}));

// crypto.randomUUID is the dispatch id source. jsdom on older Node ships
// without it; stub a deterministic value so we can assert it round-trips.
const STUB_UUID = "uuid-merge-test";
beforeEach(() => {
  i18n.changeLanguage("ja");
  enqueuePersonalityJob.mockReset();
  enqueuePersonalityMergeJob.mockReset();
  findMemoryThreadPosition.mockReset();
  enqueuePersonalityMergeJob.mockResolvedValue({ job_id_hint: "personality-merge-1" });
  getPersonality.mockResolvedValue({
    thread_count: 0,
    signal_count: 0,
    profile: null,
  });
  vi.stubGlobal("crypto", { randomUUID: () => STUB_UUID });
});

function stubProgress(): StepStreamProgressHandle {
  // Personality.tsx only reads `busy` (to swap the Generate button for a
  // Stop button) and calls `start(jobId)` on a successful dispatch — the
  // rest of the handle surface is irrelevant to the merge-button assertion.
  return {
    progress: null,
    busy: false,
    start: vi.fn(),
    clear: vi.fn(),
    cancel: vi.fn().mockResolvedValue(undefined),
  };
}

function renderPage(): { progress: StepStreamProgressHandle } {
  const progress = stubProgress();
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const ui: ReactElement = <Personality personalityProgress={progress} onNavigate={vi.fn()} />;
  render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>{ui}</QueryClientProvider>
    </I18nextProvider>,
  );
  return { progress };
}

describe("Personality merge-only button", () => {
  it("opens profile memory chips through the memory's hydrated thread_ids", async () => {
    getPersonality.mockResolvedValue({
      thread_count: 1,
      signal_count: 1,
      profile: {
        memory_id: "profile-1",
        content_json: JSON.stringify({
          interests: [
            {
              topic: "Rust",
              weight: "high",
              supporting_source_thread_ids: ["thread-a", "thread-b"],
              memory_ids: ["memory-1"],
            },
          ],
        }),
        updated_at_ms: 1_700_000_000_000,
      },
    });
    findMemoryThreadPosition.mockResolvedValue({
      thread_id: "thread-b",
      position: 3,
      thread_total: 9,
    });

    renderPage();
    await waitFor(() => expect(screen.getByText("Rust")).toBeInTheDocument());

    fireEvent.click(screen.getByRole("button", { name: "1" }));

    await waitFor(() => expect(findMemoryThreadPosition).toHaveBeenCalledTimes(1));
    expect(findMemoryThreadPosition).toHaveBeenCalledWith({ memory_id: "memory-1" });
    expect(await screen.findByText("Thread #thread-b")).toBeInTheDocument();
  });

  it("does not open a profile memory chip when the memory has no thread mapping", async () => {
    getPersonality.mockResolvedValue({
      thread_count: 1,
      signal_count: 1,
      profile: {
        memory_id: "profile-1",
        content_json: JSON.stringify({
          interests: [
            {
              topic: "Rust",
              weight: "high",
              supporting_source_thread_ids: ["thread-a"],
              memory_ids: ["memory-1"],
            },
          ],
        }),
        updated_at_ms: 1_700_000_000_000,
      },
    });
    findMemoryThreadPosition.mockResolvedValue(null);

    renderPage();
    await waitFor(() => expect(screen.getByText("Rust")).toBeInTheDocument());

    fireEvent.click(screen.getByRole("button", { name: "1" }));

    await waitFor(() => expect(findMemoryThreadPosition).toHaveBeenCalledTimes(1));
    expect(screen.queryByText(/Thread #/)).not.toBeInTheDocument();
  });

  it("does not render the temporary inventory investigation button", async () => {
    renderPage();
    await waitFor(() => expect(getPersonality).toHaveBeenCalled());

    expect(screen.queryByRole("button", { name: "在庫を調査" })).not.toBeInTheDocument();
  });

  it("dispatches enqueuePersonalityMergeJob with force_remerge=false by default", async () => {
    renderPage();
    await waitFor(() => expect(getPersonality).toHaveBeenCalled());

    fireEvent.click(screen.getByRole("button", { name: "マージのみ" }));

    await waitFor(() => expect(enqueuePersonalityMergeJob).toHaveBeenCalledTimes(1));
    expect(enqueuePersonalityMergeJob).toHaveBeenCalledWith({
      force_remerge: false,
      dispatch_id: STUB_UUID,
    });
  });

  it("forwards Force checkbox state as force_remerge=true", async () => {
    const { progress } = renderPage();
    await waitFor(() => expect(getPersonality).toHaveBeenCalled());

    // Click the Force checkbox first, then the merge button. The button
    // label flips to "Force でマージ" while the box is checked, so target
    // it by the post-check label to also verify the rename wired through.
    fireEvent.click(screen.getByLabelText(/Force 再抽出/));
    fireEvent.click(screen.getByRole("button", { name: "Force でマージ" }));

    await waitFor(() => expect(enqueuePersonalityMergeJob).toHaveBeenCalledTimes(1));
    expect(enqueuePersonalityMergeJob).toHaveBeenCalledWith({
      force_remerge: true,
      dispatch_id: STUB_UUID,
    });
    // The dispatch's job_id_hint must be threaded into the progress slot so
    // the existing personality progress hook can render `(N/M)` toasts.
    expect(progress.start).toHaveBeenCalledWith("personality-merge-1");
  });
});
