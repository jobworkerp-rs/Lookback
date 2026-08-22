import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { getReflectionSelectionContent, getSummaryContent } from "@/api";
import { Modal } from "@/components/Modal";
import { ReflectionSelectionBody } from "@/components/ReflectionSelectionBody";
import { SummarySelectionBody } from "@/components/SummarySelectionBody";
import { useSummarySelectionPages } from "@/hooks/useSummarySelectionPages";
import {
  DEFAULT_SELECTED_MEMORY_LIMITS,
  getSelectedMemoryLimits,
  normalizeSelectedMemories,
  projectSelectedMemoryContent,
} from "@/lib/selectedMemory";
import type {
  ReflectionSelectionEntry,
  SelectableMemoryKind,
  SelectedMemoryLimits,
  SelectedMemorySnapshot,
  SummaryEntry,
} from "@/types/api";

const KINDS: readonly SelectableMemoryKind[] = [
  "per-thread",
  "daily",
  "weekly",
  "monthly",
  "reflection",
];
type SelectionEntry = SummaryEntry | ReflectionSelectionEntry;
type LoadedContent = {
  content: string;
  memory_ids?: string[];
  thread_ids?: string[];
};

async function fetchSelectionContent(entry: SelectionEntry): Promise<LoadedContent> {
  if (isReflectionEntry(entry)) {
    const response = await getReflectionSelectionContent(entry.memory_id);
    if (response === null) throw new Error("reflection content is unavailable");
    return {
      content: response.content_json,
      memory_ids: response.source_memory_ids,
      thread_ids: response.source_thread_ids,
    };
  }
  const content = await getSummaryContent(entry.memory_id);
  if (content === null) throw new Error("summary content is unavailable");
  return { content };
}

function isReflectionEntry(entry: SelectionEntry): entry is ReflectionSelectionEntry {
  return "summary" in entry;
}

function toSummarySnapshot(
  entry: SummaryEntry,
  content = entry.content_json,
): SelectedMemorySnapshot {
  const { allowedMemoryIds, allowedThreadIds } = projectSelectedMemoryContent(content);
  return {
    memory_id: entry.memory_id,
    kind: entry.kind,
    content,
    period_key: entry.period_key ?? undefined,
    scope_key: entry.scope_key ?? undefined,
    source_memory_ids: allowedMemoryIds,
    source_thread_ids: entry.thread_id ? [entry.thread_id, ...allowedThreadIds] : allowedThreadIds,
    captured_at_ms: Date.now(),
  };
}

function toReflectionSnapshot(
  entry: ReflectionSelectionEntry,
  content: string,
  sourceMemoryIds: string[] = entry.source_memory_ids,
  sourceThreadIds: string[] = entry.source_thread_ids,
): SelectedMemorySnapshot {
  const projected = projectSelectedMemoryContent(content);
  return {
    memory_id: entry.memory_id,
    kind: "reflection",
    content: projected.content,
    title: entry.summary || `reflection #${entry.memory_id}`,
    source_memory_ids: [...new Set([...sourceMemoryIds, ...projected.allowedMemoryIds])],
    source_thread_ids: [
      ...new Set([
        ...(entry.origin_thread_id ? [entry.origin_thread_id] : []),
        ...sourceThreadIds,
        ...projected.allowedThreadIds,
      ]),
    ],
    captured_at_ms: Date.now(),
  };
}

function title(entry: SelectionEntry): string {
  if (isReflectionEntry(entry)) return entry.summary || `reflection #${entry.memory_id}`;
  if (entry.period_key) return `${entry.kind}: ${entry.period_key}`;
  return entry.thread_id ? `thread #${entry.thread_id}` : `memory #${entry.memory_id}`;
}

export interface SummarySelectionModalProps {
  selected: SelectedMemorySnapshot[];
  onConfirm: (selected: SelectedMemorySnapshot[]) => void;
  onClose: () => void;
}

export function SummarySelectionModal({
  selected,
  onConfirm,
  onClose,
}: SummarySelectionModalProps) {
  const { t } = useTranslation();
  const {
    entries,
    error,
    hasMore,
    isCurrentKind,
    kind,
    loadMore,
    loading,
    loadingMore,
    retry,
    selectKind,
    setError,
  } = useSummarySelectionPages();
  const [selectionLimitReached, setSelectionLimitReached] = useState(false);
  const [previewEntry, setPreviewEntry] = useState<SelectionEntry | null>(null);
  const [fullContentById, setFullContentById] = useState(() => new Map<string, string>());
  const [reflectionSourcesById, setReflectionSourcesById] = useState(
    () => new Map<string, { memory_ids: string[]; thread_ids: string[] }>(),
  );
  const [loadingMemoryIds, setLoadingMemoryIds] = useState(() => new Set<string>());
  const [draft, setDraft] = useState(() => new Map(selected.map((item) => [item.memory_id, item])));
  const [limits, setLimits] = useState<SelectedMemoryLimits>(DEFAULT_SELECTED_MEMORY_LIMITS);
  // Invalidates in-flight selection hydrations after an explicit clear.
  // Without it, a delayed response would re-add content the user removed.
  const selectionGeneration = useRef(0);
  const previewGeneration = useRef(0);
  const previewRef = useRef<HTMLElement>(null);

  useEffect(() => {
    if (!previewEntry) return;
    previewRef.current?.scrollIntoView({ block: "nearest" });
  }, [previewEntry]);

  useEffect(() => {
    let cancelled = false;
    void getSelectedMemoryLimits()
      .then((next) => {
        if (!cancelled) setLimits(next);
      })
      .catch(() => {
        // Keep the established defaults if the backend is temporarily unavailable.
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const rows = useMemo(
    () => entries.map((entry) => ({ entry, selected: draft.has(entry.memory_id) })),
    [draft, entries],
  );
  const toggle = (entry: SelectionEntry) => {
    setDraft((current) => {
      const next = new Map(current);
      if (next.has(entry.memory_id)) {
        next.delete(entry.memory_id);
        setSelectionLimitReached(false);
        return next;
      }
      if (next.size >= limits.max_selected) setSelectionLimitReached(true);
      return next;
    });
    if (draft.has(entry.memory_id) || draft.size >= limits.max_selected) return;
    const cachedContent = fullContentById.get(entry.memory_id);
    if (cachedContent !== undefined) {
      setDraft((current) => {
        if (current.has(entry.memory_id) || current.size >= limits.max_selected) return current;
        const next = new Map(current);
        const snapshot = isReflectionEntry(entry)
          ? toReflectionSnapshot(
              entry,
              cachedContent,
              reflectionSourcesById.get(entry.memory_id)?.memory_ids,
              reflectionSourcesById.get(entry.memory_id)?.thread_ids,
            )
          : toSummarySnapshot({ ...entry, content_json: cachedContent });
        next.set(entry.memory_id, snapshot);
        return next;
      });
      setSelectionLimitReached(false);
      return;
    }
    const requestGeneration = selectionGeneration.current;
    setLoadingMemoryIds((current) => new Set(current).add(entry.memory_id));
    void fetchSelectionContent(entry)
      .then((loaded) => {
        const content = loaded.content;
        setFullContentById((current) => new Map(current).set(entry.memory_id, content));
        if (isReflectionEntry(entry)) {
          setReflectionSourcesById((current) =>
            new Map(current).set(entry.memory_id, {
              memory_ids: loaded.memory_ids ?? [],
              thread_ids: loaded.thread_ids ?? [],
            }),
          );
        }
        if (requestGeneration !== selectionGeneration.current) return;
        setDraft((current) => {
          if (current.has(entry.memory_id)) return current;
          if (current.size >= limits.max_selected) {
            setSelectionLimitReached(true);
            return current;
          }
          const next = new Map(current);
          const snapshot = isReflectionEntry(entry)
            ? toReflectionSnapshot(
                entry,
                content,
                loaded.memory_ids ?? reflectionSourcesById.get(entry.memory_id)?.memory_ids,
                loaded.thread_ids ?? reflectionSourcesById.get(entry.memory_id)?.thread_ids,
              )
            : toSummarySnapshot({ ...entry, content_json: content });
          next.set(entry.memory_id, snapshot);
          return next;
        });
        setSelectionLimitReached(false);
      })
      .catch((reason) => setError(reason instanceof Error ? reason.message : String(reason)))
      .finally(() => {
        setLoadingMemoryIds((current) => {
          const next = new Set(current);
          next.delete(entry.memory_id);
          return next;
        });
      });
  };

  const preview = (entry: SelectionEntry) => {
    const requestedKind = kind;
    const requestGeneration = ++previewGeneration.current;
    setLoadingMemoryIds((current) => new Set(current).add(entry.memory_id));
    void fetchSelectionContent(entry)
      .then((loaded) => {
        if (!isCurrentKind(requestedKind) || requestGeneration !== previewGeneration.current)
          return;
        const content = loaded.content;
        setFullContentById((current) => new Map(current).set(entry.memory_id, content));
        if (isReflectionEntry(entry)) {
          setReflectionSourcesById((current) =>
            new Map(current).set(entry.memory_id, {
              memory_ids: loaded.memory_ids ?? [],
              thread_ids: loaded.thread_ids ?? [],
            }),
          );
        }
        setPreviewEntry({ ...entry, content_json: content });
      })
      .catch((reason) => {
        if (isCurrentKind(requestedKind) && requestGeneration === previewGeneration.current) {
          setError(reason instanceof Error ? reason.message : String(reason));
        }
      })
      .finally(() => {
        setLoadingMemoryIds((current) => {
          const next = new Set(current);
          next.delete(entry.memory_id);
          return next;
        });
      });
  };

  return (
    <Modal wide onClose={onClose} ariaLabel={t("chat.selectSummary.title")}>
      <div className="chat-summary-selection-modal">
        <div className="modal-header">
          <h2>{t("chat.selectSummary.title")}</h2>
          <button type="button" className="btn" onClick={onClose}>
            {t("chat.selectSummary.cancel")}
          </button>
        </div>
        <div className="chat-summary-selection-body">
          <div
            className="chat-summary-tabs"
            role="tablist"
            aria-label={t("chat.selectSummary.tabs")}
          >
            {KINDS.map((candidate) => (
              <button
                key={candidate}
                type="button"
                role="tab"
                aria-selected={candidate === kind}
                className={`btn${candidate === kind ? " primary" : ""}`}
                onClick={() => {
                  previewGeneration.current += 1;
                  setPreviewEntry(null);
                  selectKind(candidate);
                }}
              >
                {t(`chat.selectSummary.kind.${candidate}`)}
              </button>
            ))}
          </div>
          <p>{t("chat.selectSummary.count", { count: draft.size })}</p>
          {selectionLimitReached && (
            <p className="chat-error">{t("chat.selectSummary.limitReached")}</p>
          )}
          {loading && <p>{t("chat.selectSummary.loading")}</p>}
          {error && (
            <div className="chat-error">
              <p>{t("chat.selectSummary.error", { error })}</p>
              <button type="button" className="btn" onClick={retry}>
                {t("chat.selectSummary.retry")}
              </button>
            </div>
          )}
          {!loading && !error && rows.length === 0 && <p>{t("chat.selectSummary.empty")}</p>}
          <ul className="chat-summary-selection-list">
            {rows.map(({ entry, selected: checked }) => (
              <li key={entry.memory_id}>
                <label>
                  <input
                    type="checkbox"
                    checked={checked}
                    disabled={loadingMemoryIds.has(entry.memory_id)}
                    onChange={() => toggle(entry)}
                  />
                  <span>
                    {isReflectionEntry(entry) ? (
                      <>
                        <strong>{title(entry)}</strong>
                        <small>{entry.summary.slice(0, 240)}</small>
                      </>
                    ) : (
                      <SummarySelectionBody
                        raw={entry.content_json}
                        compact
                        fallbackTitle={title(entry)}
                      />
                    )}
                  </span>
                </label>
                <button
                  type="button"
                  className="btn"
                  disabled={loadingMemoryIds.has(entry.memory_id)}
                  onClick={() => preview(entry)}
                >
                  {t("chat.selectSummary.preview")}
                </button>
              </li>
            ))}
          </ul>
          {previewEntry && (
            <section
              ref={previewRef}
              className="chat-summary-preview"
              aria-label={t("chat.selectSummary.preview")}
            >
              <div className="modal-header">
                <h3>{title(previewEntry)}</h3>
                <button type="button" className="btn" onClick={() => setPreviewEntry(null)}>
                  {t("chat.selectSummary.closePreview")}
                </button>
              </div>
              <div className="chat-summary-preview-body">
                {isReflectionEntry(previewEntry) ? (
                  <ReflectionSelectionBody raw={previewEntry.content_json} />
                ) : (
                  <SummarySelectionBody raw={previewEntry.content_json} />
                )}
              </div>
            </section>
          )}
          {hasMore && !error && (
            <button type="button" className="btn" onClick={loadMore} disabled={loadingMore}>
              {t("chat.selectSummary.loadMore")}
            </button>
          )}
        </div>
        <div className="modal-actions">
          <button
            type="button"
            className="btn"
            onClick={() => {
              selectionGeneration.current += 1;
              setDraft(new Map());
            }}
          >
            {t("chat.selectSummary.clear")}
          </button>
          <button
            type="button"
            className="btn primary"
            onClick={() => onConfirm(normalizeSelectedMemories([...draft.values()], limits))}
          >
            {t("chat.selectSummary.confirm")}
          </button>
        </div>
      </div>
    </Modal>
  );
}
