import { save } from "@tauri-apps/plugin-dialog";
import type { TFunction } from "i18next";
import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { saveChatMarkdown } from "@/api";
import { MarkdownBody } from "@/components/MarkdownMessage";
import type { Route } from "@/components/Sidebar";
import { SummarySelectionModal } from "@/components/SummarySelectionModal";
import { type OpenThreadState, ThreadDetail } from "@/components/ThreadDetail";
import { Toolbar } from "@/components/Toolbar";
import type { ChatTurn, UseRagChat } from "@/hooks/useRagChat";
import { useStickToBottom } from "@/hooks/useStickToBottom";
import { resolveThreadHighlight } from "@/lib/chatSourceNav";
import { classifyPeriodKey } from "@/lib/summaryPeriod";
import { synthesizeThreadSummary } from "@/lib/threadSummary";
import type { SummariesFocus } from "@/pages/Summaries";
import type { ChatSource, RetrievalMode, SelectedMemorySnapshot } from "@/types/api";

export interface ChatPageProps {
  /** Plain tab switch — used as a fallback when a period-summary pill's
   *  `period_key` can't be classified into a calendar tuple (so the user
   *  at least lands on the Summaries tab instead of nowhere). */
  onNavigate: (route: Route) => void;
  /** Deep-link variant: routes to the Summaries tab AND seeds it with
   *  the calendar kind / month / selected period so the user lands on
   *  the cited period card. App.tsx clears the seed once consumed. */
  onNavigateSummariesFocus: (focus: SummariesFocus) => void;
  /** Conversation state lifted to `App.tsx` so tab switches don't
   *  unmount the `useTauriEvent` listener or wipe turns. */
  rag: UseRagChat;
}

export function Chat({ onNavigate, onNavigateSummariesFocus, rag }: ChatPageProps) {
  const { t } = useTranslation();
  const { turns, ask, reset, cancel, busy } = rag;
  const [draft, setDraft] = useState("");
  const [selectedMemories, setSelectedMemories] = useState<SelectedMemorySnapshot[]>([]);
  const [retrievalMode, setRetrievalMode] = useState<RetrievalMode>("source_details");
  const [selectionOpen, setSelectionOpen] = useState(false);
  const clearChat = useCallback(() => {
    reset();
    setSelectedMemories([]);
  }, [reset]);
  const [openThread, setOpenThread] = useState<OpenThreadState | null>(null);
  // Bumped on every source-pill click so stale `resolveThreadHighlight`
  // resolutions can detect they're no longer the active request and
  // skip their `setOpenThread` — protects against rapid alternating
  // clicks where the slower IPC reply would otherwise win.
  const sourceClickToken = useRef(0);

  const { containerRef, isPinnedAway, scrollToBottom, notifyContentChanged } = useStickToBottom();
  // Tail-length is the streaming trigger; the hook is a no-op when the
  // user has scrolled away from the bottom.
  // biome-ignore lint/correctness/useExhaustiveDependencies: token deltas are the trigger; the tail's answer length captures them
  useEffect(() => {
    notifyContentChanged();
  }, [turns.length, turns[turns.length - 1]?.answer.length, notifyContentChanged]);

  const submit = useCallback(async () => {
    const text = draft.trim();
    if (!text || busy) return;
    setDraft("");
    // Submitting a new question always lands the user at the stream,
    // regardless of where they'd scrolled to read prior turns.
    scrollToBottom();
    try {
      await ask(text, selectedMemories, selectedMemories.length > 0 ? retrievalMode : undefined);
    } catch (e) {
      // `chat_ask` rejected before the stream could start (sidecar
      // down, validation error, etc.). `useRagChat.ask` has already
      // emitted an error event onto the turn it pre-registered, so
      // the failure shows up in the conversation log — but the user
      // still lost their typed draft. Restore it so they don't have
      // to retype to retry.
      setDraft(text);
      console.error("chat_ask failed", e);
    }
  }, [draft, busy, ask, retrievalMode, scrollToBottom, selectedMemories]);

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      // ⌘Enter (mac) / Ctrl+Enter submits — Enter alone inserts a
      // newline, matching the input convention of every other Lookback
      // textarea (ImportDialog, SummaryGenerateDialog).
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        void submit();
      }
    },
    [submit],
  );

  const copyAnswer = useCallback(async (answer: string) => {
    await navigator.clipboard.writeText(answer);
  }, []);

  const saveAnswer = useCallback(async (answer: string) => {
    const path = await save({
      defaultPath: "lookback-draft.md",
      filters: [{ name: "Markdown", extensions: ["md"] }],
    });
    if (!path) return false;
    await saveChatMarkdown(path, answer);
    return true;
  }, []);

  const onSourceClick = useCallback(
    async (source: ChatSource) => {
      if (source.source_kind === "period_summary") {
        // Period summaries don't have a single origin thread, but they
        // do carry the calendar coordinates (period_key / scope_key)
        // the Summaries tab needs to surface the cited card. Classify
        // first; only fall back to a plain tab switch if the token
        // shape is unrecognised (defensive — the workflow only emits
        // daily/weekly/monthly tokens, but the fallback keeps the user
        // from landing on nothing).
        const cls = classifyPeriodKey(source.period_key);
        if (cls) {
          onNavigateSummariesFocus({
            kind: cls.kind,
            month: cls.month,
            periodKey: source.period_key,
          });
        } else {
          onNavigate("summaries");
        }
        return;
      }
      const threadId = source.source_thread_id;
      const memoryId = source.source_kind === "reflection" ? null : source.memory_id;
      const thread = synthesizeThreadSummary({ id: threadId });
      const myToken = ++sourceClickToken.current;
      const highlight = memoryId ? await resolveThreadHighlight(threadId, memoryId) : undefined;
      if (myToken !== sourceClickToken.current) {
        // A later click superseded ours while we awaited the highlight.
        return;
      }
      setOpenThread({ thread, highlight });
    },
    [onNavigate, onNavigateSummariesFocus],
  );

  return (
    <div className="chat-page">
      <Toolbar
        title={t("chat.title")}
        subtitle={t("chat.subtitle")}
        actions={
          <button
            type="button"
            className="btn"
            onClick={clearChat}
            disabled={turns.length === 0 || busy}
            title={t("chat.clearTitle")}
          >
            {t("chat.clear")}
          </button>
        }
      />

      <div className="chat-scroll" ref={containerRef}>
        {turns.length === 0 && <ChatEmptyState />}
        {turns.map((turn) => (
          <ChatTurnCard
            key={turn.jobId}
            turn={turn}
            onSourceClick={onSourceClick}
            onCopy={copyAnswer}
            onSave={saveAnswer}
          />
        ))}
        {isPinnedAway && (
          // Inside .chat-scroll so `position: sticky` pins it to the
          // viewport's bottom while the user reads earlier turns.
          <button
            type="button"
            className="chat-jump-to-latest"
            onClick={scrollToBottom}
            title={t("chat.jumpLatestTitle")}
          >
            {t("chat.jumpLatest")}
          </button>
        )}
      </div>

      <ChatComposer
        draft={draft}
        onDraftChange={setDraft}
        onSubmit={submit}
        onCancel={cancel}
        onKeyDown={onKeyDown}
        busy={busy}
        selectedMemories={selectedMemories}
        retrievalMode={retrievalMode}
        onOpenSelection={() => setSelectionOpen(true)}
        onRetrievalModeChange={setRetrievalMode}
        onRemoveSelected={(memoryId) =>
          setSelectedMemories((current) => current.filter((item) => item.memory_id !== memoryId))
        }
      />

      {openThread && (
        <ThreadDetail
          thread={openThread.thread}
          highlight={openThread.highlight}
          onClose={() => setOpenThread(null)}
        />
      )}
      {selectionOpen && (
        <SummarySelectionModal
          selected={selectedMemories}
          onClose={() => setSelectionOpen(false)}
          onConfirm={(next) => {
            setSelectedMemories(next);
            setSelectionOpen(false);
          }}
        />
      )}
    </div>
  );
}

/** Build an export that keeps selected summaries distinct from retrieved sources. */
export interface ChatMarkdownLabels {
  selectedSummaries: string;
  sources: string;
  kind: string;
  memoryId: string;
  memory: (id: string) => string;
  selectedKind: (kind: SelectedMemorySnapshot["kind"]) => string;
  sourceKind: (kind: ChatSource["source_kind"]) => string;
}

/** Build an export with caller-provided, localized labels. */
export function buildChatMarkdown(
  turn: ChatTurn,
  includeSources: boolean,
  labels: ChatMarkdownLabels,
): string {
  if (!includeSources) return turn.answer;
  const sections = [turn.answer];
  if (turn.selectedMemories && turn.selectedMemories.length > 0) {
    sections.push(
      `## ${labels.selectedSummaries}\n\n${turn.selectedMemories
        .map((item) => {
          const label = item.title ?? item.period_key ?? labels.memory(item.memory_id);
          return `### ${label}\n\n- ${labels.kind}: ${labels.selectedKind(item.kind)}\n- ${labels.memoryId}: ${item.memory_id}`;
        })
        .join("\n\n")}`,
    );
  }
  if (turn.sources.length > 0) {
    sections.push(
      `## ${labels.sources}\n\n${turn.sources
        .map((source) => `- ${labels.sourceKind(source.source_kind)}: ${source.snippet}`)
        .join("\n")}`,
    );
  }
  return sections.join("\n\n");
}

function ChatEmptyState() {
  const { t } = useTranslation();
  return (
    <div className="chat-empty">
      <p>{t("chat.empty.line1")}</p>
      <p style={{ fontSize: 12, color: "var(--label-secondary)" }}>{t("chat.empty.line2")}</p>
    </div>
  );
}

interface ChatTurnCardProps {
  turn: ChatTurn;
  onSourceClick: (source: ChatSource) => void;
  onCopy: (answer: string) => Promise<void>;
  onSave: (answer: string) => Promise<boolean>;
}

const ChatTurnCard = memo(function ChatTurnCard({
  turn,
  onSourceClick,
  onCopy,
  onSave,
}: ChatTurnCardProps) {
  const { t } = useTranslation();
  const markdownLabels = useMemo<ChatMarkdownLabels>(
    () => ({
      selectedSummaries: t("chat.export.selectedSummaries"),
      sources: t("chat.export.sources"),
      kind: t("chat.export.kind"),
      memoryId: t("chat.export.memoryId"),
      memory: (id) => t("chat.export.memory", { id }),
      selectedKind: (kind) => t(`chat.selectSummary.kind.${kind}`),
      sourceKind: (kind) => t(`chat.sourceKind.${kind}`),
    }),
    [t],
  );
  const [actionStatus, setActionStatus] = useState<"copy" | "save" | "error" | null>(null);
  const [includeSources, setIncludeSources] = useState(false);
  const runAction = useCallback(
    async (action: "copy" | "save") => {
      try {
        const markdown = buildChatMarkdown(turn, includeSources, markdownLabels);
        if (action === "copy") {
          await onCopy(markdown);
          setActionStatus(action);
          return;
        }
        if (await onSave(markdown)) setActionStatus(action);
      } catch {
        setActionStatus("error");
      }
    },
    [includeSources, markdownLabels, onCopy, onSave, turn],
  );
  // Auto-scroll is owned by the parent's `useStickToBottom` so the
  // viewport tracks the token stream only while the user is still at
  // the bottom — scrolling up to read earlier output no longer gets
  // yanked back on every token.
  return (
    <article className="chat-turn">
      <div className="chat-question">
        <strong>{t("chat.you")}</strong>
        <p>{turn.question}</p>
        {turn.selectedMemories && turn.selectedMemories.length > 0 && (
          <details className="chat-selected-used">
            <summary className="chat-selected-summary">
              {t("chat.selected", { count: turn.selectedMemories.length })}
            </summary>
            {turn.selectedContentTruncated && <p>{t("chat.selectedContentTruncated")}</p>}
            <ul>
              {turn.selectedMemories.map((item) => (
                <li key={item.memory_id}>
                  <strong>{item.title ?? item.period_key ?? item.memory_id}</strong>
                  <span className="chat-selected-kind">
                    {t(`chat.selectSummary.kind.${item.kind}`)}
                  </span>
                  <p>{item.content}</p>
                </li>
              ))}
            </ul>
          </details>
        )}
      </div>
      <div className="chat-answer">
        <strong>Lookback</strong>
        <ChatAnswerBody turn={turn} />
        {turn.phase === "done" && !turn.cancelled && turn.answer && (
          <div className="chat-answer-actions">
            {(turn.sources.length > 0 || (turn.selectedMemories?.length ?? 0) > 0) && (
              <label className="chat-answer-source-option">
                <input
                  type="checkbox"
                  checked={includeSources}
                  onChange={(event) => setIncludeSources(event.target.checked)}
                />
                {t("chat.answer.includeSources")}
              </label>
            )}
            <button type="button" className="btn" onClick={() => void runAction("copy")}>
              {t("chat.answer.copy")}
            </button>
            <button type="button" className="btn" onClick={() => void runAction("save")}>
              {t("chat.answer.save")}
            </button>
            {actionStatus && (
              <span className="chat-answer-action-status" role="status">
                {actionStatus === "copy"
                  ? t("chat.answer.copySuccess")
                  : actionStatus === "save"
                    ? t("chat.answer.saveSuccess")
                    : t("chat.answer.actionFailed")}
              </span>
            )}
          </div>
        )}
        {turn.sources.length > 0 && (
          <ChatSourceList sources={turn.sources} onSourceClick={onSourceClick} />
        )}
      </div>
    </article>
  );
});

function ChatAnswerBody({ turn }: { turn: ChatTurn }) {
  const { t } = useTranslation();
  if (turn.error) {
    return (
      <div className="chat-error">
        <span style={{ color: "var(--danger)" }}>{t("chat.errorPrefix")}</span> {turn.error}
      </div>
    );
  }
  if (turn.phase === "start") {
    return <ChatStatus text={t("chat.status.preparing")} />;
  }
  if (turn.phase === "searching" && !turn.answer) {
    // Empty answer + searching = the LLM hasn't started generating
    // yet, it's still waiting on the tool result. Surface the message
    // verbatim so we don't claim "generating" before the LLM is.
    return <ChatStatus text={turn.message ?? t("chat.status.searching")} />;
  }
  if (turn.phase === "done" && !turn.answer) {
    return <ChatStatus text={turn.message ?? t("chat.status.noAnswer")} />;
  }
  if (!turn.answer) {
    return <ChatStatus text={t("chat.status.generating")} />;
  }
  // Wrap in `.message-body` so heading/list spacing matches the
  // reflection cards and thread message bubbles.
  return (
    <div className="message-body">
      <MarkdownBody>{turn.answer}</MarkdownBody>
    </div>
  );
}

function ChatStatus({ text }: { text: string }) {
  // A single static line that doubles as a typing indicator. Animation
  // would compete with the streaming token render for attention; the
  // text changes (searching → generating → done) are signal enough.
  return <div className="chat-status">{text}</div>;
}

interface ChatSourceListProps {
  sources: ChatSource[];
  onSourceClick: (source: ChatSource) => void;
}

function ChatSourceList({ sources, onSourceClick }: ChatSourceListProps) {
  const { t } = useTranslation();
  // 10 entries × 2 snippet lines was eating most of the viewport, so
  // the list is collapsed by default and the summary line carries a
  // per-kind breakdown so the user can decide if it's worth opening.
  // useMemo because the streaming turn re-renders on every token while
  // its sources are stable.
  const breakdown = useMemo(() => summarizeKinds(t, sources), [t, sources]);
  return (
    <details className="chat-sources">
      <summary className="chat-sources-summary">
        <span className="chat-sources-label">
          {t("chat.sources.label", { count: sources.length })}
        </span>
        {breakdown && <span className="chat-sources-breakdown">{breakdown}</span>}
      </summary>
      <ul>
        {sources.map((src) => (
          <ChatSourceItem key={sourceKey(src)} source={src} onSourceClick={onSourceClick} />
        ))}
      </ul>
    </details>
  );
}

function sourceKey(source: ChatSource): string {
  return source.source_kind === "reflection"
    ? `reflection-${source.reflection_id}`
    : `memory-${source.memory_id}`;
}

/** Render the kind-count breakdown (e.g. "Raw 7 / Thread summary 2 / Period summary 1")
 *  for the collapsed source summary. Returns null when only one kind is
 *  present — repeating the same label as `Sources (N)` would be noise.
 *  Iteration order follows SOURCE_KIND_ORDER so the canonical kind sequence
 *  (raw → thread summary → period summary) is the single source of truth. */
function summarizeKinds(t: TFunction, sources: ChatSource[]): string | null {
  const counts = new Map<ChatSource["source_kind"], number>();
  for (const src of sources) {
    counts.set(src.source_kind, (counts.get(src.source_kind) ?? 0) + 1);
  }
  const parts = SOURCE_KIND_ORDER.filter((kind) => (counts.get(kind) ?? 0) > 0).map(
    (kind) => `${t(`chat.sourceKind.${kind}`)} ${counts.get(kind)}`,
  );
  return parts.length > 1 ? parts.join(" / ") : null;
}

interface ChatSourceItemProps {
  source: ChatSource;
  onSourceClick: (source: ChatSource) => void;
}

function ChatSourceItem({ source, onSourceClick }: ChatSourceItemProps) {
  const { t } = useTranslation();
  const label = t(`chat.sourceKind.${source.source_kind}`);
  // `period_summary` aggregates many threads (no single origin) and
  // jumps to the Summaries tab; the others have a concrete thread and
  // open in-place via `ThreadDetail`. The detail text mirrors the same
  // distinction.
  const detail =
    source.source_kind === "period_summary"
      ? `${source.period_key} (${source.scope_key})`
      : t("chat.sources.threadDetail", { id: source.source_thread_id });
  return (
    <li className="chat-source">
      <button
        type="button"
        className="chat-source-pill"
        onClick={() => void onSourceClick(source)}
        title={`${label}: ${detail}`}
      >
        <span className={`chat-source-kind chat-source-kind--${source.source_kind}`}>{label}</span>
        <span className="chat-source-detail">{detail}</span>
        <span className="chat-source-score">{source.score.toFixed(2)}</span>
      </button>
      <p className="chat-source-snippet">{source.snippet}</p>
    </li>
  );
}

// Canonical kind order for the collapsed-source breakdown; the label itself is
// resolved via `t("chat.sourceKind.<kind>")`.
const SOURCE_KIND_ORDER: ChatSource["source_kind"][] = [
  "raw_memory",
  "thread_summary",
  "period_summary",
  "reflection",
];

interface ChatComposerProps {
  draft: string;
  onDraftChange: (v: string) => void;
  onSubmit: () => void | Promise<void>;
  onCancel: () => void | Promise<void>;
  onKeyDown: (e: React.KeyboardEvent<HTMLTextAreaElement>) => void;
  busy: boolean;
  selectedMemories: SelectedMemorySnapshot[];
  retrievalMode: RetrievalMode;
  onOpenSelection: () => void;
  onRetrievalModeChange: (mode: RetrievalMode) => void;
  onRemoveSelected: (memoryId: string) => void;
}

function ChatComposer({
  draft,
  onDraftChange,
  onSubmit,
  onCancel,
  onKeyDown,
  busy,
  selectedMemories,
  retrievalMode,
  onOpenSelection,
  onRetrievalModeChange,
  onRemoveSelected,
}: ChatComposerProps) {
  const { t } = useTranslation();
  return (
    <div className="chat-composer">
      <div className="chat-composer-tools">
        <button type="button" className="btn" onClick={onOpenSelection} disabled={busy}>
          {t("chat.composer.selectSummary")} ({selectedMemories.length})
        </button>
        {selectedMemories.length > 0 && (
          <label className="chat-mode-select">
            <span>{t("chat.composer.mode")}</span>
            <select
              value={retrievalMode}
              onChange={(event) => onRetrievalModeChange(event.target.value as RetrievalMode)}
              disabled={busy}
            >
              <option value="selected_only">{t("chat.composer.modeSelectedOnly")}</option>
              <option value="source_details">{t("chat.composer.modeSourceDetails")}</option>
              <option value="related_memories">{t("chat.composer.modeRelated")}</option>
            </select>
          </label>
        )}
      </div>
      {selectedMemories.length > 0 && (
        <fieldset className="chat-selected-chips">
          <legend>{t("chat.selected", { count: selectedMemories.length })}</legend>
          {selectedMemories.map((item) => (
            <button
              key={item.memory_id}
              type="button"
              className="chat-selected-chip"
              onClick={() => onRemoveSelected(item.memory_id)}
              disabled={busy}
              title={t("chat.composer.removeSummary")}
            >
              {item.period_key ?? item.memory_id} ×
            </button>
          ))}
        </fieldset>
      )}
      <textarea
        className="chat-input"
        placeholder={t("chat.composer.placeholder")}
        value={draft}
        onChange={(e) => onDraftChange(e.target.value)}
        onKeyDown={onKeyDown}
        rows={3}
        disabled={busy}
      />
      {busy ? (
        // Stop replaces Submit while in flight (OPEN-CHAT-2). Single
        // button slot keeps the composer layout stable and avoids two
        // mutually-exclusive primary actions sitting side by side.
        <button type="button" className="btn secondary" onClick={() => void onCancel()}>
          {t("chat.composer.stop")}
        </button>
      ) : (
        <button
          type="button"
          className="btn primary"
          onClick={() => void onSubmit()}
          disabled={draft.trim().length === 0}
        >
          {t("chat.composer.send")}
        </button>
      )}
    </div>
  );
}
