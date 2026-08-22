import { useTranslation } from "react-i18next";
import { MarkdownBody } from "@/components/MarkdownMessage";
import { MemoryRefs } from "@/components/MemoryRefs";
import {
  asSummaryObject,
  parseSummarySelection,
  summarySelectionExcerptFromParsed,
  summaryStrings,
  summaryText,
} from "@/lib/summarySelectionContent";
import type { SummaryValue } from "@/types/api";

const KNOWN_FIELDS = new Set([
  "title",
  "summary",
  "overall_purpose",
  "category",
  "status",
  "key_decisions",
  "purpose_groups",
  "by_topic",
  "carryover",
  "trends",
  "highlights",
  "milestones",
  "source_memory_ids",
  "source_thread_ids",
]);

const CARD_FIELDS = new Set([
  "purpose",
  "topic",
  "title",
  "status",
  "kind",
  "summary",
  "outcome",
  "completed_in_week",
  "bullets",
  "source_memory_ids",
]);

const FIELD_KEYS: Record<string, string> = {
  title: "title",
  summary: "summary",
  overall_purpose: "overallPurpose",
  category: "category",
  status: "status",
  key_decisions: "keyDecisions",
  purpose_groups: "purposeGroups",
  by_topic: "byTopic",
  carryover: "carryover",
  trends: "trends",
  highlights: "highlights",
  milestones: "milestones",
  source_memory_ids: "sourceMemoryIds",
  source_thread_ids: "sourceThreadIds",
  purpose: "purpose",
  bullets: "bullets",
  topic: "topic",
  kind: "kind",
  outcome: "outcome",
  completed_in_week: "completedInWeek",
};

export function SummarySelectionBody({
  raw,
  compact = false,
  fallbackTitle,
}: {
  raw: string;
  compact?: boolean;
  fallbackTitle?: string;
}) {
  const { t } = useTranslation();
  const parsed = parseSummarySelection(raw);
  const excerpt = summarySelectionExcerptFromParsed(parsed, raw);
  if (compact) {
    return (
      <div className="summary-selection-compact" data-testid="summary-selection-compact">
        <strong>{excerpt.title ?? fallbackTitle}</strong>
        {excerpt.summary && <span className="summary-selection-excerpt">{excerpt.summary}</span>}
        <SummaryMetadataTags category={excerpt.category} status={excerpt.status} />
        {excerpt.decisions.length > 0 && (
          <ul className="summary-selection-decisions">
            {excerpt.decisions.map((decision, index) => (
              // Decisions can repeat verbatim when source threads converge.
              // biome-ignore lint/suspicious/noArrayIndexKey: duplicates require a positional key
              <li key={`${index}-${decision}`}>{decision}</li>
            ))}
          </ul>
        )}
      </div>
    );
  }
  if (!parsed) {
    return (
      <div className="message-body">
        <MarkdownBody>{raw}</MarkdownBody>
      </div>
    );
  }
  const title = excerpt.title;
  return (
    <div className="summary-selection-body">
      {title && <h4>{title}</h4>}
      <SummaryMetadataTags category={excerpt.category} status={excerpt.status} />
      <KnownSection
        title={t("chat.selectSummary.summaryFields.summary")}
        value={parsed.summary}
        markdown
      />
      <KnownSection
        title={t("chat.selectSummary.summaryFields.overallPurpose")}
        value={parsed.overall_purpose}
        markdown
      />
      <KnownSection
        title={t("chat.selectSummary.summaryFields.keyDecisions")}
        value={parsed.key_decisions}
      />
      {(["purpose_groups", "by_topic", "trends", "highlights", "milestones"] as const).map(
        (key) => (
          <StructuredCollection key={key} title={fieldLabel(t, key)} value={parsed[key]} />
        ),
      )}
      <KnownSection title={fieldLabel(t, "carryover")} value={parsed.carryover} />
      <References
        title={fieldLabel(t, "source_memory_ids")}
        ids={summaryStrings(parsed.source_memory_ids)}
      />
      <References
        title={fieldLabel(t, "source_thread_ids")}
        ids={summaryStrings(parsed.source_thread_ids)}
        thread
      />
      {Object.entries(parsed)
        .filter(([key]) => !KNOWN_FIELDS.has(key) && key !== "タイトル" && key !== "件名")
        .map(([key, value]) => (
          <KnownSection key={key} title={humanize(key)} value={value} />
        ))}
    </div>
  );
}

function SummaryMetadataTags({
  category,
  status,
}: {
  category: string | null;
  status: string | null;
}) {
  const { t } = useTranslation();
  if (!category && !status) return null;
  return (
    <div className="summary-selection-tags">
      {category && (
        <Tag label={t("chat.selectSummary.summaryFields.category")}>
          {translatedValue(t, "category", category)}
        </Tag>
      )}
      {status && (
        <Tag label={t("chat.selectSummary.summaryFields.status")}>
          {translatedValue(t, "status", status)}
        </Tag>
      )}
    </div>
  );
}

function Tag({ children, label }: { children: string; label?: string }) {
  return (
    <span className="summary-selection-tag">{label ? `${label}: ${children}` : children}</span>
  );
}

function translatedValue(
  t: ReturnType<typeof useTranslation>["t"],
  group: "category" | "status" | "trendKind",
  value: string,
) {
  const key = `chat.selectSummary.${group}.${value}`;
  const translated = t(key);
  return translated === key ? humanize(value) : translated;
}

function fieldLabel(t: ReturnType<typeof useTranslation>["t"], field: string) {
  const key = FIELD_KEYS[field];
  return key ? t(`chat.selectSummary.summaryFields.${key}`) : humanize(field);
}

function humanize(key: string) {
  return key.replaceAll("_", " ");
}

function References({
  title,
  ids,
  thread = false,
}: {
  title: string;
  ids: string[];
  thread?: boolean;
}) {
  if (ids.length === 0) return null;
  return (
    <section className="summary-selection-section">
      <div>{title}</div>
      <MemoryRefs ids={ids} titlePrefix={thread ? "thread" : "memory"} />
    </section>
  );
}

function KnownSection({
  title,
  value,
  markdown = false,
}: {
  title: string;
  value: SummaryValue | undefined;
  markdown?: boolean;
}) {
  if (value == null || value === "" || (Array.isArray(value) && value.length === 0)) return null;
  return (
    <section className="summary-selection-section">
      <div>{title}</div>
      <Value value={value} markdown={markdown} />
    </section>
  );
}

function Value({ value, markdown = false }: { value: SummaryValue; markdown?: boolean }) {
  if (typeof value === "string")
    return markdown ? (
      <div className="message-body">
        <MarkdownBody>{value}</MarkdownBody>
      </div>
    ) : (
      <span>{value}</span>
    );
  if (typeof value === "number" || typeof value === "boolean") return <span>{String(value)}</span>;
  if (Array.isArray(value))
    return (
      <ul>
        {value.map((item, index) => (
          // Ordered summary bullets may intentionally repeat the same text.
          // biome-ignore lint/suspicious/noArrayIndexKey: duplicates require a positional key
          <li key={`${index}-${JSON.stringify(item)}`}>
            <Value value={item} />
          </li>
        ))}
      </ul>
    );
  if (value && typeof value === "object")
    return (
      <div className="summary-selection-nested">
        {Object.entries(value).map(([key, item]) =>
          key === "source_memory_ids" ? (
            <References key={key} title={humanize(key)} ids={summaryStrings(item)} />
          ) : key === "source_thread_ids" ? (
            <References key={key} title={humanize(key)} ids={summaryStrings(item)} thread />
          ) : (
            <div key={key}>
              <span>{humanize(key)}</span>
              <Value value={item} markdown={key === "summary" || key === "detail"} />
            </div>
          ),
        )}
      </div>
    );
  return null;
}

function StructuredCollection({
  title,
  value,
}: {
  title: string;
  value: SummaryValue | undefined;
}) {
  const { t } = useTranslation();
  if (!Array.isArray(value) || value.length === 0) return null;
  return (
    <section className="summary-selection-section">
      <div>{title}</div>
      <div className="summary-selection-cards">
        {value.map((item, index) => {
          const card = asSummaryObject(item);
          if (!card) {
            // Ordered summary cards may intentionally repeat identical content.
            // biome-ignore lint/suspicious/noArrayIndexKey: duplicates require a positional key
            return <Value key={`${index}-${JSON.stringify(item)}`} value={item} />;
          }
          const heading =
            summaryText(card.purpose) ?? summaryText(card.topic) ?? summaryText(card.title);
          const status = summaryText(card.status);
          const trendKind = summaryText(card.kind);
          const summary = summaryText(card.summary);
          const outcome = summaryText(card.outcome);
          const completedInWeek = summaryText(card.completed_in_week);
          return (
            // Ordered summary cards may intentionally repeat identical content.
            // biome-ignore lint/suspicious/noArrayIndexKey: duplicates require a positional key
            <article className="summary-selection-card" key={`${index}-${JSON.stringify(card)}`}>
              {heading && <h5>{heading}</h5>}
              {(status || trendKind) && (
                <div className="summary-selection-tags">
                  {status && (
                    <Tag label={fieldLabel(t, "status")}>
                      {translatedValue(t, "status", status)}
                    </Tag>
                  )}
                  {trendKind && (
                    <Tag label={fieldLabel(t, "kind")}>
                      {translatedValue(t, "trendKind", trendKind)}
                    </Tag>
                  )}
                </div>
              )}
              {summary && (
                <div className="message-body">
                  <MarkdownBody>{summary}</MarkdownBody>
                </div>
              )}
              {outcome && (
                <div>
                  <span>{fieldLabel(t, "outcome")}: </span>
                  {outcome}
                </div>
              )}
              {completedInWeek && (
                <div>
                  <span>{fieldLabel(t, "completed_in_week")}: </span>
                  {completedInWeek}
                </div>
              )}
              {summaryStrings(card.bullets).length > 0 && (
                <KnownSection title={fieldLabel(t, "bullets")} value={card.bullets} />
              )}
              <References
                title={fieldLabel(t, "source_memory_ids")}
                ids={summaryStrings(card.source_memory_ids)}
              />
              {Object.entries(card)
                .filter(([key]) => !CARD_FIELDS.has(key))
                .map(([key, extra]) => (
                  <KnownSection key={key} title={humanize(key)} value={extra} />
                ))}
            </article>
          );
        })}
      </div>
    </section>
  );
}
