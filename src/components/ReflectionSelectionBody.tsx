import { useTranslation } from "react-i18next";
import { MarkdownBody } from "@/components/MarkdownMessage";
import { outcomeLabel, reflectionAspectLabel, taskCategoryLabel } from "@/lib/searchTaxonomy";

interface ReflectionObject {
  [key: string]: unknown;
}

const FAILURE_MODE_KEYS: Record<number, string> = {
  0: "unspecified",
  1: "toolMisuse",
  2: "loop",
  3: "scopeDrift",
  4: "hallucination",
  5: "contextOverflow",
  6: "dataLoss",
  7: "permissionIssue",
  8: "ambiguousInstruction",
  9: "conflictingRequirements",
  10: "missingContext",
  11: "misleadingPremise",
  12: "goalDriftByUser",
  13: "toolUnavailable",
  14: "externalServiceFailure",
  15: "rateLimit",
  16: "other",
};

function asObject(value: unknown): ReflectionObject | null {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as ReflectionObject)
    : null;
}

function text(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function strings(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === "string")
    : [];
}

function numbers(value: unknown): number[] {
  return Array.isArray(value)
    ? value.filter((item): item is number => typeof item === "number" && Number.isFinite(item))
    : [];
}

/** Structured, read-only rendering for canonical reflection JSON. Enum
 * values are translated before display so users never have to interpret
 * protobuf numbers, while malformed fields degrade to a short Markdown body. */
export function ReflectionSelectionBody({ raw }: { raw: string }) {
  const { t } = useTranslation();
  let parsed: ReflectionObject | null = null;
  try {
    parsed = asObject(JSON.parse(raw));
  } catch {
    // Legacy or malformed content is still readable as Markdown below.
  }
  if (!parsed)
    return (
      <div className="message-body">
        <MarkdownBody>{raw}</MarkdownBody>
      </div>
    );

  const summary = text(parsed.summary);
  const intent = text(parsed.task_intent);
  const lessons = [...new Set(strings(parsed.lessons))];
  const decisions = [...new Set(strings(parsed.key_decisions))];
  const successFactors = [...new Set(strings(parsed.success_factors))];
  const tools = [...new Set(strings(parsed.tools_used))];
  const mitigationHint = text(parsed.mitigation_hint);
  const failureModes = [...new Set(numbers(parsed.failure_modes))];
  const failureModesOther = [...new Set(strings(parsed.failure_modes_other))];
  const facts = Array.isArray(parsed.facts)
    ? parsed.facts.map(asObject).filter((fact): fact is ReflectionObject => fact !== null)
    : [];
  const category = typeof parsed.task_category === "number" ? parsed.task_category : 0;
  const aspect = typeof parsed.reflection_aspect === "number" ? parsed.reflection_aspect : 0;
  const outcome = typeof parsed.outcome === "number" ? parsed.outcome : 0;

  return (
    <div className="reflection-selection-body">
      <div className="reflection-card-head">
        <span className="reflection-tag">{taskCategoryLabel(t, category)}</span>
        <span className="reflection-tag">{outcomeLabel(t, outcome)}</span>
        <span className="reflection-tag">{reflectionAspectLabel(t, aspect)}</span>
        {typeof parsed.score === "number" && (
          <span className="reflection-score">{parsed.score.toFixed(2)}</span>
        )}
      </div>
      {intent && <ReflectionSection title={t("reflections.card.taskIntent")} markdown={intent} />}
      {summary && <ReflectionSection title={t("reflections.card.summary")} markdown={summary} />}
      <ReflectionList title={t("reflections.card.lessons")} items={lessons} />
      <ReflectionList title={t("reflections.card.keyDecisions")} items={decisions} />
      <ReflectionList title={t("reflections.card.successFactors")} items={successFactors} />
      <ReflectionList title={t("reflections.card.tools")} items={tools} />
      {(failureModes.length > 0 || failureModesOther.length > 0) && (
        <div className="reflection-section">
          <div className="reflection-section-title">{t("reflections.card.failureModes")}</div>
          <ul className="reflection-section-list">
            {failureModes.map((mode) => (
              <li key={mode}>
                {t(`taxonomy.failureMode.${FAILURE_MODE_KEYS[mode] ?? "unknown"}`)}
              </li>
            ))}
            {failureModesOther.map((mode) => (
              <li key={mode}>{mode}</li>
            ))}
          </ul>
        </div>
      )}
      {mitigationHint && (
        <ReflectionSection title={t("reflections.card.mitigationHint")} markdown={mitigationHint} />
      )}
      {facts.length > 0 && (
        <div className="reflection-section">
          <div className="reflection-section-title">{t("reflections.card.facts")}</div>
          <ul className="reflection-section-list">
            {facts.map((fact) => (
              <li key={JSON.stringify(fact)}>
                {text(fact.note) || text(fact.kind) || t("reflections.card.factWithoutNote")}
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}

function ReflectionSection({ title, markdown }: { title: string; markdown: string }) {
  return (
    <div className="reflection-section">
      <div className="reflection-section-title">{title}</div>
      <div className="message-body">
        <MarkdownBody>{markdown}</MarkdownBody>
      </div>
    </div>
  );
}

function ReflectionList({ title, items }: { title: string; items: string[] }) {
  if (items.length === 0) return null;
  return (
    <div className="reflection-section">
      <div className="reflection-section-title">{title}</div>
      <ul className="reflection-section-list message-body">
        {items.map((item) => (
          <li key={item}>
            <MarkdownBody>{item}</MarkdownBody>
          </li>
        ))}
      </ul>
    </div>
  );
}
