import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { codingAgentAttachRun, codingAgentListEvidence, codingAgentReadEvidence } from "@/api";
import type { CodingAgentEvidenceReference, CodingAgentRunStatus } from "@/types/api";
import { type CodingAgentAgentReport, decodeAgentReport } from "./assessment";

const MAX_EVIDENCE_VIEW_BYTES = 1024 * 1024;
const MAX_COPY_TEXT_BYTES = 64 * 1024;
const MAX_BINARY_PREVIEW_BYTES = 256;

type EvidenceView =
  | {
      runId: string;
      evidenceId: string;
      referenceKey: string;
      kind: "text";
      text: string;
      byteLength: number;
    }
  | {
      runId: string;
      evidenceId: string;
      referenceKey: string;
      kind: "binary";
      hex: string;
      byteLength: number;
      omittedBytes: number;
    }
  | {
      runId: string;
      evidenceId: string;
      referenceKey: string;
      kind: "report";
      report: CodingAgentAgentReport | null;
      rawBytes: Uint8Array;
      byteLength: number;
    }
  | { runId: string; evidenceId: string; referenceKey: string; kind: "error"; message: string };

type ActiveEvidenceRead = {
  requestId: number;
  runId: string;
  evidenceId: string;
  referenceKey: string;
};

function evidenceReferenceKey(reference: CodingAgentEvidenceReference): string {
  return JSON.stringify([
    reference.evidenceId,
    reference.attemptId,
    reference.name,
    reference.mediaType,
    reference.sha256,
    reference.sizeBytes,
    reference.saveConfirmed,
    reference.createdAtMs,
  ]);
}

function mayContainCredentials(reference: CodingAgentEvidenceReference): boolean {
  return /(?:^|[-_.])(?:auth|credential|secret|token|login|key|password)(?:[-_.]|$)/i.test(
    `${reference.name} ${reference.mediaType}`,
  );
}

function isTextMediaType(mediaType: string): boolean {
  return (
    mediaType.startsWith("text/") ||
    ["application/json", "application/xml", "application/yaml", "application/toml"].includes(
      mediaType.toLowerCase(),
    )
  );
}

function downloadOriginalReport(bytes: Uint8Array): void {
  const exactBytes = new ArrayBuffer(bytes.byteLength);
  new Uint8Array(exactBytes).set(bytes);
  const url = URL.createObjectURL(new Blob([exactBytes], { type: "application/octet-stream" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = "agent-report.raw";
  link.rel = "noopener";
  link.click();
  window.setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export function CodingAgentEvidenceViewer({ run }: { run: CodingAgentRunStatus }) {
  const { t } = useTranslation();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [readingId, setReadingId] = useState<string | null>(null);
  const [view, setView] = useState<EvidenceView | null>(null);
  const [copied, setCopied] = useState(false);
  const readRequestId = useRef(0);
  const activeRead = useRef<ActiveEvidenceRead | null>(null);
  const latestRunId = useRef(run.runId);
  const previousRunId = useRef(run.runId);
  latestRunId.current = run.runId;
  useEffect(() => {
    if (previousRunId.current !== run.runId) {
      previousRunId.current = run.runId;
      readRequestId.current += 1;
      activeRead.current = null;
      setSelectedId(null);
      setReadingId(null);
      setView(null);
      setCopied(false);
    }
    return () => {
      readRequestId.current += 1;
      activeRead.current = null;
    };
  }, [run.runId]);
  const evidenceQuery = useQuery({
    queryKey: ["coding-agent-evidence", run.runId],
    retry: false,
    queryFn: async () => {
      const owner = await codingAgentAttachRun(run.runId);
      if (owner.runId !== run.runId) throw new Error("run owner response mismatch");
      return codingAgentListEvidence(run.runId);
    },
  });
  const refs = evidenceQuery.data ?? [];
  useEffect(() => {
    const currentRead = activeRead.current;
    if (!currentRead) return;
    const reference = evidenceQuery.data?.find(
      (candidate) => candidate.evidenceId === currentRead.evidenceId,
    );
    if (
      currentRead.runId !== run.runId ||
      !reference ||
      evidenceReferenceKey(reference) !== currentRead.referenceKey
    ) {
      readRequestId.current += 1;
      activeRead.current = null;
      setReadingId(null);
      setView(null);
    }
  }, [evidenceQuery.data, run.runId]);
  const selected = refs.find((reference) => reference.evidenceId === selectedId);
  const visibleView =
    view?.runId === run.runId &&
    view.evidenceId === selectedId &&
    selected &&
    view.referenceKey === evidenceReferenceKey(selected)
      ? view
      : null;

  const readEvidence = async (reference: CodingAgentEvidenceReference) => {
    if (
      !reference.saveConfirmed ||
      !Number.isSafeInteger(reference.sizeBytes) ||
      reference.sizeBytes < 0 ||
      reference.sizeBytes > MAX_EVIDENCE_VIEW_BYTES ||
      mayContainCredentials(reference)
    ) {
      return;
    }
    const requestId = ++readRequestId.current;
    const requestRunId = run.runId;
    const referenceKey = evidenceReferenceKey(reference);
    activeRead.current = {
      requestId,
      runId: requestRunId,
      evidenceId: reference.evidenceId,
      referenceKey,
    };
    const isCurrentRequest = () =>
      readRequestId.current === requestId &&
      latestRunId.current === requestRunId &&
      activeRead.current?.requestId === requestId &&
      activeRead.current.runId === requestRunId &&
      activeRead.current.evidenceId === reference.evidenceId &&
      activeRead.current.referenceKey === referenceKey;
    setSelectedId(reference.evidenceId);
    setView(null);
    setReadingId(reference.evidenceId);
    setCopied(false);
    try {
      const response = await codingAgentReadEvidence(run.runId, reference.evidenceId);
      if (
        response.evidenceId !== reference.evidenceId ||
        response.mediaType !== reference.mediaType ||
        !Array.isArray(response.bytes) ||
        response.bytes.length !== reference.sizeBytes ||
        response.bytes.length > MAX_EVIDENCE_VIEW_BYTES ||
        response.bytes.some((byte) => !Number.isInteger(byte) || byte < 0 || byte > 255)
      ) {
        throw new Error("invalid evidence response");
      }
      const bytes = Uint8Array.from(response.bytes);
      if (reference.name === "agent-report") {
        const parsed = decodeAgentReport(bytes);
        const expectedTaskKind =
          run.purpose === "implementation" ? "implementation" : "investigation";
        const report =
          parsed?.run_id === run.runId &&
          parsed.attempt_id === reference.attemptId &&
          parsed.task_kind === expectedTaskKind
            ? parsed
            : null;
        if (isCurrentRequest()) {
          setView({
            runId: requestRunId,
            evidenceId: reference.evidenceId,
            referenceKey,
            kind: "report",
            report,
            rawBytes: bytes,
            byteLength: bytes.byteLength,
          });
        }
      } else if (isTextMediaType(response.mediaType)) {
        let text: string;
        try {
          text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
        } catch {
          if (isCurrentRequest()) {
            setView({
              runId: requestRunId,
              evidenceId: reference.evidenceId,
              referenceKey,
              kind: "error",
              message: t("codingAgent.evidence.invalidText"),
            });
          }
          return;
        }
        if (isCurrentRequest()) {
          setView({
            runId: requestRunId,
            evidenceId: reference.evidenceId,
            referenceKey,
            kind: "text",
            text,
            byteLength: bytes.byteLength,
          });
        }
      } else {
        const preview = bytes.subarray(0, MAX_BINARY_PREVIEW_BYTES);
        if (isCurrentRequest()) {
          setView({
            runId: requestRunId,
            evidenceId: reference.evidenceId,
            referenceKey,
            kind: "binary",
            hex: Array.from(preview, (byte) => byte.toString(16).padStart(2, "0")).join(" "),
            byteLength: bytes.byteLength,
            omittedBytes: Math.max(0, bytes.byteLength - preview.byteLength),
          });
        }
      }
    } catch {
      if (isCurrentRequest()) {
        setView({
          runId: requestRunId,
          evidenceId: reference.evidenceId,
          referenceKey,
          kind: "error",
          message: t("codingAgent.evidence.readUnavailable"),
        });
      }
    } finally {
      if (isCurrentRequest()) {
        activeRead.current = null;
        setReadingId(null);
      }
    }
  };

  const copyText = async () => {
    if (visibleView?.kind !== "text" || visibleView.byteLength > MAX_COPY_TEXT_BYTES) {
      return;
    }
    try {
      await navigator.clipboard.writeText(visibleView.text);
      setCopied(true);
    } catch {
      setView({
        runId: run.runId,
        evidenceId: visibleView.evidenceId,
        referenceKey: visibleView.referenceKey,
        kind: "error",
        message: t("codingAgent.evidence.copyUnavailable"),
      });
    }
  };

  return (
    <section aria-label={t("codingAgent.evidence.title")} className="coding-agent-evidence">
      <h4>{t("codingAgent.evidence.title")}</h4>
      <p className="settings-card-desc">{t("codingAgent.evidence.sensitivityNotice")}</p>
      {evidenceQuery.isPending ? (
        <p role="status">{t("common.loading")}</p>
      ) : evidenceQuery.isError ? (
        <p role="alert" className="settings-error">
          {t("codingAgent.evidence.listUnavailable")}
        </p>
      ) : refs.length === 0 ? (
        <p role="status">{t("codingAgent.evidence.none")}</p>
      ) : (
        <ul>
          {refs.map((reference) => {
            const sensitive = mayContainCredentials(reference);
            const oversized =
              !Number.isSafeInteger(reference.sizeBytes) ||
              reference.sizeBytes < 0 ||
              reference.sizeBytes > MAX_EVIDENCE_VIEW_BYTES;
            const readable = reference.saveConfirmed && !sensitive && !oversized;
            return (
              <li key={reference.evidenceId} style={{ marginBottom: 8 }}>
                <span>{reference.name}</span>
                {" · "}
                <span>{reference.mediaType}</span>
                {" · "}
                <span>{t("codingAgent.evidence.size", { bytes: reference.sizeBytes })}</span>
                {" · "}
                <span>
                  {reference.saveConfirmed
                    ? t("codingAgent.run.evidenceSaved")
                    : t("codingAgent.evidence.notSaved")}
                </span>
                {" · sha256 "}
                <code>{reference.sha256}</code>
                {sensitive ? (
                  <p role="status">{t("codingAgent.evidence.sensitiveWithheld")}</p>
                ) : oversized ? (
                  <p role="status">{t("codingAgent.evidence.tooLarge")}</p>
                ) : (
                  <button
                    type="button"
                    className="btn"
                    disabled={!readable || readingId === reference.evidenceId}
                    onClick={() => {
                      setSelectedId(reference.evidenceId);
                      void readEvidence(reference);
                    }}
                  >
                    {readingId === reference.evidenceId
                      ? t("codingAgent.evidence.reading")
                      : t("codingAgent.evidence.read")}
                  </button>
                )}
              </li>
            );
          })}
        </ul>
      )}
      <p className="settings-card-desc">{t("codingAgent.evidence.paginationUnavailable")}</p>
      {selected && visibleView && (
        <div>
          <h5>{selected.name}</h5>
          {visibleView.kind === "text" ? (
            <>
              <pre style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>
                {visibleView.text}
              </pre>
              {visibleView.byteLength <= MAX_COPY_TEXT_BYTES && (
                <button type="button" className="btn" onClick={() => void copyText()}>
                  {copied ? t("codingAgent.evidence.copied") : t("codingAgent.evidence.copy")}
                </button>
              )}
              {visibleView.byteLength > MAX_COPY_TEXT_BYTES && (
                <p role="status">{t("codingAgent.evidence.copyLimit")}</p>
              )}
            </>
          ) : visibleView.kind === "binary" ? (
            <p role="status">
              {t("codingAgent.evidence.binaryPreview", {
                size: visibleView.byteLength,
                hex: visibleView.hex || t("codingAgent.evidence.emptyBinary"),
                omitted: visibleView.omittedBytes,
              })}
            </p>
          ) : visibleView.kind === "report" ? (
            visibleView.report ? (
              <section
                aria-label={t("codingAgent.evidence.reportSummary", {
                  defaultValue: "Structured agent report",
                })}
              >
                <p role="status">
                  {t("codingAgent.evidence.reportStatus", {
                    defaultValue: "Agent-reported status",
                  })}
                  : {visibleView.report.status}.{" "}
                  {t("codingAgent.evidence.reportUntrusted", {
                    defaultValue: "This is self-reported data, not a trusted success result.",
                  })}
                </p>
                <p>{visibleView.report.summary}</p>
                <h6>
                  {t("codingAgent.evidence.reportAcceptance", {
                    defaultValue: "Acceptance claims",
                  })}
                </h6>
                <ul>
                  {visibleView.report.acceptance.map((item) => (
                    <li key={item.criterion_id}>
                      {item.criterion_id} · {item.assessment}
                      {item.evidence.length > 0 && ` · ${item.evidence.join(" · ")}`}
                    </li>
                  ))}
                </ul>
                <h6>
                  {t("codingAgent.evidence.reportValidation", {
                    defaultValue: "Validation claims",
                  })}
                </h6>
                <ul>
                  {visibleView.report.validation.map((item) => (
                    <li key={item.validation_id}>
                      {item.validation_id} · {item.claimed_result}
                      {item.reason ? ` · ${item.reason}` : ""}
                      {item.evidence.length > 0 && ` · ${item.evidence.join(" · ")}`}
                    </li>
                  ))}
                </ul>
                {visibleView.report.unresolved.length > 0 && (
                  <>
                    <h6>
                      {t("codingAgent.evidence.reportUnresolved", {
                        defaultValue: "Unresolved items",
                      })}
                    </h6>
                    <ul>
                      {visibleView.report.unresolved.map((item, index) => (
                        // biome-ignore lint/suspicious/noArrayIndexKey: unresolved claims have no stable ID in schema v1.
                        <li key={`${index}:${item.description}`}>
                          {item.blocks_completion ? "Blocks completion · " : ""}
                          {item.description}
                        </li>
                      ))}
                    </ul>
                  </>
                )}
                {visibleView.report.investigation && (
                  <div>
                    <h6>
                      {t("codingAgent.evidence.reportFindings", {
                        defaultValue: "Investigation findings",
                      })}
                    </h6>
                    <ul>
                      {visibleView.report.investigation.findings.map((finding, index) => (
                        // biome-ignore lint/suspicious/noArrayIndexKey: report findings have no stable ID in schema v1.
                        <li key={`${index}:${finding.summary}`}>
                          {finding.summary}
                          {finding.criterion_ids.length > 0 &&
                            ` · ${finding.criterion_ids.join(", ")}`}
                          {finding.evidence.length > 0 && ` · ${finding.evidence.join(" · ")}`}
                        </li>
                      ))}
                    </ul>
                  </div>
                )}
                {visibleView.report.implementation && (
                  <p>
                    {t("codingAgent.evidence.reportChangeSummary", {
                      defaultValue: "Reported change summary",
                    })}
                    : {visibleView.report.implementation.change_summary}
                  </p>
                )}
              </section>
            ) : (
              <p role="alert" className="settings-error">
                {t("codingAgent.evidence.invalidReport", {
                  defaultValue: "Invalid schema-v1 report; structured status is unavailable.",
                })}
              </p>
            )
          ) : (
            <p role="alert" className="settings-error">
              {visibleView.message}
            </p>
          )}
          {visibleView.kind === "report" && (
            <button
              type="button"
              className="btn"
              onClick={() => {
                try {
                  downloadOriginalReport(visibleView.rawBytes);
                } catch {
                  setView({
                    runId: run.runId,
                    evidenceId: visibleView.evidenceId,
                    referenceKey: visibleView.referenceKey,
                    kind: "error",
                    message: t("codingAgent.evidence.downloadUnavailable", {
                      defaultValue: "Could not download original report bytes.",
                    }),
                  });
                }
              }}
            >
              {t("codingAgent.evidence.downloadReport", {
                defaultValue: "Download original report bytes",
              })}
            </button>
          )}
        </div>
      )}
    </section>
  );
}
