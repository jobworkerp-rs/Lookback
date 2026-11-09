import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { codingAgentAttachRun, codingAgentListEvidence, codingAgentReadEvidence } from "@/api";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { CodingAgentEvidenceReference, CodingAgentRunStatus } from "@/types/api";
import { CodingAgentEvidenceViewer } from "./CodingAgentEvidenceViewer";

vi.mock("@/api", () => ({
  codingAgentAttachRun: vi.fn(),
  codingAgentListEvidence: vi.fn(),
  codingAgentReadEvidence: vi.fn(),
}));

const run: CodingAgentRunStatus = {
  runId: "run-evidence",
  issueId: "issue-evidence",
  issueRevision: 2,
  revision: 4,
  executionMode: "one_shot",
  purpose: "implementation",
  state: "awaiting_review",
  stage: "review",
  profileId: "profile-evidence",
  profileVersion: 1,
  currentAttemptId: "attempt-evidence",
  autoDone: false,
  stdoutBytes: 0,
  stderrBytes: 0,
  evidence: [],
};

function evidenceReference(
  evidenceId: string,
  name: string,
  mediaType: string,
  sizeBytes: number,
): CodingAgentEvidenceReference {
  return {
    evidenceId,
    attemptId: "attempt-evidence",
    name,
    mediaType,
    sha256: "b".repeat(64),
    sizeBytes,
    saveConfirmed: true,
    createdAtMs: 100,
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function readBlobBytes(blob: Blob): Promise<Uint8Array> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error);
    reader.onload = () => {
      if (!(reader.result instanceof ArrayBuffer)) {
        reject(new Error("Blob could not be read as bytes"));
        return;
      }
      resolve(new Uint8Array(reader.result));
    };
    reader.readAsArrayBuffer(blob);
  });
}

describe("CodingAgentEvidenceViewer", () => {
  beforeEach(() => {
    i18n.changeLanguage("en");
    vi.clearAllMocks();
    vi.mocked(codingAgentAttachRun).mockImplementation(async (runId) => ({
      runId,
      windowLabel: "main",
    }));
    vi.mocked(codingAgentListEvidence).mockResolvedValue([
      evidenceReference(
        "evidence-text",
        "validation-report.txt",
        "text/plain",
        new TextEncoder().encode('<img src=x onerror="alert(1)">').byteLength,
      ),
      evidenceReference("evidence-binary", "artifact.bin", "application/octet-stream", 3),
      evidenceReference("evidence-auth", "auth-session.log", "text/plain", 20),
      evidenceReference("evidence-large", "large-report.txt", "text/plain", 1_048_577),
    ]);
    vi.mocked(codingAgentReadEvidence).mockImplementation(async (_runId, evidenceId) => ({
      evidenceId,
      mediaType: evidenceId === "evidence-binary" ? "application/octet-stream" : "text/plain",
      bytes: Array.from(new TextEncoder().encode('<img src=x onerror="alert(1)">')),
    }));
  });

  it("reads only bounded save-confirmed evidence on explicit request and renders text as inert React content", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    renderWithProviders(<CodingAgentEvidenceViewer run={run} />);
    const viewer = await screen.findByRole("region", { name: "Coding-agent evidence" });
    expect(codingAgentAttachRun).toHaveBeenCalledWith("run-evidence");
    expect(codingAgentListEvidence).toHaveBeenCalledWith("run-evidence");
    expect(await within(viewer).findByText("validation-report.txt")).toBeInTheDocument();

    const textRow = within(viewer).getByText("validation-report.txt").closest("li");
    if (!textRow) throw new Error("text evidence row is missing");
    fireEvent.click(within(textRow).getByRole("button", { name: /Read bounded evidence/ }));
    await waitFor(() =>
      expect(codingAgentReadEvidence).toHaveBeenCalledWith("run-evidence", "evidence-text"),
    );
    expect(await screen.findByText('<img src=x onerror="alert(1)">')).toBeInTheDocument();
    expect(screen.queryByRole("img")).not.toBeInTheDocument();
    fireEvent.click(within(viewer).getByRole("button", { name: "Copy bounded text" }));
    await waitFor(() => expect(writeText).toHaveBeenCalledWith('<img src=x onerror="alert(1)">'));
  });

  it("keeps the newest evidence read active and ignores late earlier responses", async () => {
    const firstText = "first evidence response";
    const secondText = "second evidence response";
    const firstBytes = Array.from(new TextEncoder().encode(firstText));
    const secondBytes = Array.from(new TextEncoder().encode(secondText));
    const firstRead = deferred<{
      evidenceId: string;
      mediaType: string;
      bytes: number[];
    }>();
    const secondRead = deferred<{
      evidenceId: string;
      mediaType: string;
      bytes: number[];
    }>();
    vi.mocked(codingAgentListEvidence).mockResolvedValue([
      evidenceReference("evidence-first", "first.txt", "text/plain", firstBytes.length),
      evidenceReference("evidence-second", "second.txt", "text/plain", secondBytes.length),
    ]);
    vi.mocked(codingAgentReadEvidence).mockImplementation((_runId, evidenceId) =>
      evidenceId === "evidence-first" ? firstRead.promise : secondRead.promise,
    );
    renderWithProviders(<CodingAgentEvidenceViewer run={run} />);
    const viewer = await screen.findByRole("region", { name: "Coding-agent evidence" });
    const firstRow = (await within(viewer).findByText("first.txt")).closest("li");
    const secondRow = (await within(viewer).findByText("second.txt")).closest("li");
    if (!firstRow || !secondRow) throw new Error("evidence rows are missing");

    fireEvent.click(within(firstRow).getByRole("button", { name: /Read bounded evidence/ }));
    await waitFor(() =>
      expect(codingAgentReadEvidence).toHaveBeenCalledWith("run-evidence", "evidence-first"),
    );
    fireEvent.click(within(secondRow).getByRole("button", { name: /Read bounded evidence/ }));
    await waitFor(() =>
      expect(codingAgentReadEvidence).toHaveBeenCalledWith("run-evidence", "evidence-second"),
    );
    expect(within(secondRow).getByRole("button", { name: /Reading evidence/ })).toBeDisabled();

    await act(async () => {
      firstRead.resolve({
        evidenceId: "evidence-first",
        mediaType: "text/plain",
        bytes: firstBytes,
      });
      await firstRead.promise;
    });

    expect(within(secondRow).getByRole("button", { name: /Reading evidence/ })).toBeDisabled();
    expect(screen.queryByText(firstText)).not.toBeInTheDocument();
    expect(screen.queryByText(secondText)).not.toBeInTheDocument();

    await act(async () => {
      secondRead.resolve({
        evidenceId: "evidence-second",
        mediaType: "text/plain",
        bytes: secondBytes,
      });
      await secondRead.promise;
    });
    expect(await screen.findByText(secondText)).toBeInTheDocument();
    expect(screen.queryByText(firstText)).not.toBeInTheDocument();
  });

  it("does not let an earlier read failure clear a newer read's busy state", async () => {
    const secondText = "current evidence response";
    const secondBytes = Array.from(new TextEncoder().encode(secondText));
    const firstRead = deferred<{
      evidenceId: string;
      mediaType: string;
      bytes: number[];
    }>();
    const secondRead = deferred<{
      evidenceId: string;
      mediaType: string;
      bytes: number[];
    }>();
    vi.mocked(codingAgentListEvidence).mockResolvedValue([
      evidenceReference("evidence-first", "first.txt", "text/plain", 4),
      evidenceReference("evidence-second", "second.txt", "text/plain", secondBytes.length),
    ]);
    vi.mocked(codingAgentReadEvidence).mockImplementation((_runId, evidenceId) =>
      evidenceId === "evidence-first" ? firstRead.promise : secondRead.promise,
    );
    renderWithProviders(<CodingAgentEvidenceViewer run={run} />);
    const viewer = await screen.findByRole("region", { name: "Coding-agent evidence" });
    const firstRow = (await within(viewer).findByText("first.txt")).closest("li");
    const secondRow = (await within(viewer).findByText("second.txt")).closest("li");
    if (!firstRow || !secondRow) throw new Error("evidence rows are missing");
    fireEvent.click(within(firstRow).getByRole("button", { name: /Read bounded evidence/ }));
    await waitFor(() => expect(codingAgentReadEvidence).toHaveBeenCalledTimes(1));
    fireEvent.click(within(secondRow).getByRole("button", { name: /Read bounded evidence/ }));
    await waitFor(() => expect(codingAgentReadEvidence).toHaveBeenCalledTimes(2));

    await act(async () => {
      firstRead.reject(new Error("earlier read failed"));
      await firstRead.promise.catch(() => undefined);
    });

    expect(within(secondRow).getByRole("button", { name: /Reading evidence/ })).toBeDisabled();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    await act(async () => {
      secondRead.resolve({
        evidenceId: "evidence-second",
        mediaType: "text/plain",
        bytes: secondBytes,
      });
      await secondRead.promise;
    });
    expect(await screen.findByText(secondText)).toBeInTheDocument();
  });

  it("does not replace a completed newest view with a late earlier read", async () => {
    const firstText = "late evidence response";
    const secondText = "latest completed evidence";
    const firstBytes = Array.from(new TextEncoder().encode(firstText));
    const secondBytes = Array.from(new TextEncoder().encode(secondText));
    const firstRead = deferred<{
      evidenceId: string;
      mediaType: string;
      bytes: number[];
    }>();
    const secondRead = deferred<{
      evidenceId: string;
      mediaType: string;
      bytes: number[];
    }>();
    vi.mocked(codingAgentListEvidence).mockResolvedValue([
      evidenceReference("evidence-first", "first.txt", "text/plain", firstBytes.length),
      evidenceReference("evidence-second", "second.txt", "text/plain", secondBytes.length),
    ]);
    vi.mocked(codingAgentReadEvidence).mockImplementation((_runId, evidenceId) =>
      evidenceId === "evidence-first" ? firstRead.promise : secondRead.promise,
    );
    renderWithProviders(<CodingAgentEvidenceViewer run={run} />);
    const viewer = await screen.findByRole("region", { name: "Coding-agent evidence" });
    const firstRow = (await within(viewer).findByText("first.txt")).closest("li");
    const secondRow = (await within(viewer).findByText("second.txt")).closest("li");
    if (!firstRow || !secondRow) throw new Error("evidence rows are missing");
    fireEvent.click(within(firstRow).getByRole("button", { name: /Read bounded evidence/ }));
    await waitFor(() => expect(codingAgentReadEvidence).toHaveBeenCalledTimes(1));
    fireEvent.click(within(secondRow).getByRole("button", { name: /Read bounded evidence/ }));
    await waitFor(() => expect(codingAgentReadEvidence).toHaveBeenCalledTimes(2));

    await act(async () => {
      secondRead.resolve({
        evidenceId: "evidence-second",
        mediaType: "text/plain",
        bytes: secondBytes,
      });
      await secondRead.promise;
    });
    expect(await screen.findByText(secondText)).toBeInTheDocument();
    await act(async () => {
      firstRead.resolve({
        evidenceId: "evidence-first",
        mediaType: "text/plain",
        bytes: firstBytes,
      });
      await firstRead.promise;
    });
    expect(screen.getByText(secondText)).toBeInTheDocument();
    expect(screen.queryByText(firstText)).not.toBeInTheDocument();
  });

  it("shows binary bytes as a bounded hex preview, reports invalid UTF-8 and withholds sensitive or oversized items", async () => {
    vi.mocked(codingAgentReadEvidence).mockImplementation(async (_runId, evidenceId) => ({
      evidenceId,
      mediaType: evidenceId === "evidence-invalid" ? "text/plain" : "application/octet-stream",
      bytes: evidenceId === "evidence-invalid" ? [0xff] : [0, 255, 16],
    }));
    vi.mocked(codingAgentListEvidence).mockResolvedValue([
      evidenceReference("evidence-binary", "artifact.bin", "application/octet-stream", 3),
      evidenceReference("evidence-invalid", "broken.txt", "text/plain", 1),
      evidenceReference("evidence-auth", "auth-session.log", "text/plain", 20),
      evidenceReference("evidence-large", "large-report.txt", "text/plain", 1_048_577),
    ]);
    renderWithProviders(<CodingAgentEvidenceViewer run={run} />);
    const viewer = await screen.findByRole("region", { name: "Coding-agent evidence" });

    expect(
      await within(viewer).findByText(
        /Potentially sensitive credential or authentication evidence/,
      ),
    ).toBeInTheDocument();
    expect(within(viewer).getByText(/exceeds the display limit/)).toBeInTheDocument();
    expect(within(viewer).queryByRole("button", { name: /auth-session/ })).not.toBeInTheDocument();
    const readableItems = within(viewer).getAllByRole("button", { name: /Read bounded evidence/ });
    const firstReadableItem = readableItems[0];
    if (!firstReadableItem) throw new Error("binary evidence is missing its read button");
    fireEvent.click(firstReadableItem);
    expect(await screen.findByText(/Hex preview: 00 ff 10/)).toBeInTheDocument();

    const brokenRow = within(viewer).getByText("broken.txt").closest("li");
    if (!brokenRow) throw new Error("invalid text evidence row is missing");
    fireEvent.click(within(brokenRow).getByRole("button", { name: /Read bounded evidence/ }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/not valid UTF-8|UTF-8/);
    expect(codingAgentReadEvidence).toHaveBeenCalledTimes(2);
    expect(codingAgentReadEvidence).not.toHaveBeenCalledWith("run-evidence", "evidence-auth");
    expect(codingAgentReadEvidence).not.toHaveBeenCalledWith("run-evidence", "evidence-large");
  });

  it("shows bounded schema-v1 report claims as untrusted status and keeps the exact raw bytes downloadable", async () => {
    const report = {
      schema_version: 1,
      run_id: "run-evidence",
      attempt_id: "attempt-evidence",
      task_kind: "implementation",
      status: "blocked",
      summary: "The required validation is unavailable.",
      acceptance: [
        { criterion_id: "AC-1", assessment: "unknown", evidence: ["No proof was emitted."] },
      ],
      validation: [
        {
          validation_id: "V-1",
          claimed_result: "not_run",
          evidence: [],
          reason: "No approved command was available.",
        },
      ],
      investigation: null,
      implementation: {
        change_summary: "No changes.",
        changed_files: [],
        implementation_notes: [],
      },
      unresolved: [
        {
          description: "The required check remains unavailable.",
          blocks_completion: true,
          criterion_ids: ["AC-1"],
        },
      ],
    };
    const bytes = new TextEncoder().encode(JSON.stringify(report));
    vi.mocked(codingAgentListEvidence).mockResolvedValue([
      evidenceReference(
        "evidence-report",
        "agent-report",
        "application/octet-stream",
        bytes.length,
      ),
    ]);
    vi.mocked(codingAgentReadEvidence).mockResolvedValue({
      evidenceId: "evidence-report",
      mediaType: "application/octet-stream",
      bytes: Array.from(bytes),
    });
    const createObjectURL = vi.fn((_blob: Blob) => "blob:agent-report");
    Object.defineProperty(URL, "createObjectURL", { configurable: true, value: createObjectURL });
    const anchorClick = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    renderWithProviders(<CodingAgentEvidenceViewer run={run} />);
    const viewer = await screen.findByRole("region", { name: "Coding-agent evidence" });
    const reportRow = (await within(viewer).findByText("agent-report")).closest("li");
    if (!reportRow) throw new Error("agent report evidence row is missing");
    fireEvent.click(within(reportRow).getByRole("button", { name: /Read bounded evidence/ }));

    expect(await screen.findByRole("status")).toHaveTextContent("blocked");
    expect(screen.getByRole("status")).toHaveTextContent(/Agent-reported status/);
    expect(screen.getByRole("status")).toHaveTextContent(/not a trusted success result/);
    expect(screen.getByText("The required validation is unavailable.")).toBeInTheDocument();
    const reportItems = within(viewer)
      .getAllByRole("listitem")
      .map((item) => item.textContent ?? "");
    expect(reportItems.find((item) => item.includes("AC-1"))).toContain("unknown");
    expect(reportItems.find((item) => item.includes("V-1"))).toContain("not_run");
    expect(screen.getByText(/The required check remains unavailable/)).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /Download original report bytes/ }));
    await waitFor(() => expect(createObjectURL).toHaveBeenCalledTimes(1));
    const blob = createObjectURL.mock.calls[0]?.[0];
    if (!blob) throw new Error("original report Blob was not created");
    expect(blob).toBeInstanceOf(Blob);
    const downloadedBytes = await readBlobBytes(blob);
    expect(Array.from(downloadedBytes)).toEqual(Array.from(bytes));
    expect(anchorClick).toHaveBeenCalledTimes(1);
  });

  it("marks malformed report-shaped evidence invalid without dropping its raw download", async () => {
    const malformed = new TextEncoder().encode('{"schema_version":2,"status":"completed"}');
    vi.mocked(codingAgentListEvidence).mockResolvedValue([
      evidenceReference(
        "evidence-report",
        "agent-report",
        "application/octet-stream",
        malformed.length,
      ),
    ]);
    vi.mocked(codingAgentReadEvidence).mockResolvedValue({
      evidenceId: "evidence-report",
      mediaType: "application/octet-stream",
      bytes: Array.from(malformed),
    });
    renderWithProviders(<CodingAgentEvidenceViewer run={run} />);
    const row = (await screen.findByText("agent-report")).closest("li");
    if (!row) throw new Error("agent report evidence row is missing");
    fireEvent.click(within(row).getByRole("button", { name: /Read bounded evidence/ }));

    expect(await screen.findByRole("alert")).toHaveTextContent(/Invalid schema-v1 report/);
    expect(screen.getByRole("button", { name: /Download original report bytes/ })).toBeEnabled();
  });

  it("rejects duplicate-status reports without rendering claims and downloads their exact original bytes", async () => {
    const report = {
      schema_version: 1,
      run_id: run.runId,
      attempt_id: "attempt-evidence",
      task_kind: "implementation",
      status: "blocked",
      summary: '<img src=x onerror="alert(1)">',
      acceptance: [],
      validation: [],
      investigation: null,
      implementation: {
        change_summary: "No changes.",
        changed_files: [],
        implementation_notes: [],
      },
      unresolved: [],
    };
    const validJson = JSON.stringify(report);
    const rawBytes = new TextEncoder().encode(
      validJson.replace('"status":"blocked",', '"status":"blocked","status":"completed",'),
    );
    vi.mocked(codingAgentListEvidence).mockResolvedValue([
      evidenceReference(
        "evidence-report",
        "agent-report",
        "application/octet-stream",
        rawBytes.length,
      ),
    ]);
    vi.mocked(codingAgentReadEvidence).mockResolvedValue({
      evidenceId: "evidence-report",
      mediaType: "application/octet-stream",
      bytes: Array.from(rawBytes),
    });
    const createObjectURL = vi.fn((_blob: Blob) => "blob:duplicate-report");
    Object.defineProperty(URL, "createObjectURL", { configurable: true, value: createObjectURL });
    Object.defineProperty(URL, "revokeObjectURL", {
      configurable: true,
      value: vi.fn(),
    });
    const anchorClick = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    renderWithProviders(<CodingAgentEvidenceViewer run={run} />);
    const row = (await screen.findByText("agent-report")).closest("li");
    if (!row) throw new Error("agent report evidence row is missing");
    fireEvent.click(within(row).getByRole("button", { name: /Read bounded evidence/ }));

    expect(await screen.findByRole("alert")).toHaveTextContent(/Invalid schema-v1 report/);
    expect(screen.queryByText(/Agent-reported status/)).not.toBeInTheDocument();
    expect(screen.queryByRole("img")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /Download original report bytes/ }));
    await waitFor(() => expect(createObjectURL).toHaveBeenCalledTimes(1));
    const blob = createObjectURL.mock.calls[0]?.[0];
    if (!blob) throw new Error("duplicate report Blob was not created");
    expect(Array.from(await readBlobBytes(blob))).toEqual(Array.from(rawBytes));
    expect(anchorClick).toHaveBeenCalledTimes(1);
  });
});
