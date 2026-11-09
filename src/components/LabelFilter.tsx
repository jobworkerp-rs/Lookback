import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { CATEGORY_GROUP_KEY, groupByPrefix } from "@/lib/labelPrefix";
import type { LabelMatch } from "@/types/api";

export interface LabelFilterItem {
  label: string;
  count?: number;
}

export interface LabelFilterProps {
  labels: LabelFilterItem[];
  /** When set, narrows the rendered list to selected ∪ co-occurring labels. */
  coOccurringLabels?: LabelFilterItem[];
  selected: string[];
  match: LabelMatch;
  onToggle: (label: string) => void;
  onToggleMany: (labels: string[], turnOn: boolean) => void;
  onSetMatch: (m: LabelMatch) => void;
  /** Hides the AND/OR control when the embedding page fixes the match mode. */
  showMatchToggle?: boolean;
  onClear?: () => void;
  isLoading?: boolean;
}

export function LabelFilter({
  labels,
  coOccurringLabels,
  selected,
  match,
  onToggle,
  onToggleMany,
  onSetMatch,
  showMatchToggle = true,
  onClear,
  isLoading,
}: LabelFilterProps) {
  const { t } = useTranslation();
  const [localOpen, setLocalOpen] = useState(false);
  const selectedSet = useMemo(() => new Set(selected), [selected]);

  // When narrowed, swap each chip's count to the co-occurrence count
  // (threads also carrying the current selection), since the user's next
  // click would restrict to that intersection — showing the global count
  // overstates what's available. The proto excludes input labels from the
  // co-occurring response, so selected labels keep their distinct count.
  const effective = useMemo(() => {
    if (!coOccurringLabels) return labels;
    const coCount = new Map(coOccurringLabels.map((l) => [l.label, l.count]));
    const out: LabelFilterItem[] = [];
    for (const l of labels) {
      if (selectedSet.has(l.label)) {
        out.push(l);
      } else if (coCount.has(l.label)) {
        out.push({ label: l.label, count: coCount.get(l.label) });
      }
    }
    return out;
  }, [labels, coOccurringLabels, selectedSet]);

  const groups = useMemo(() => groupByPrefix(effective), [effective]);

  if (isLoading) {
    return <div className="label-filter-bar label-filter-empty">{t("labelFilter.loading")}</div>;
  }
  if (labels.length === 0) return null;

  const open = localOpen || selected.length > 0;
  const canSetMatch = showMatchToggle && selected.length >= 2;

  return (
    <details
      className="label-filter-fold"
      open={open}
      onToggle={(e) => setLocalOpen((e.currentTarget as HTMLDetailsElement).open)}
    >
      <summary className="label-filter-fold-summary" title={selected.join(", ") || undefined}>
        <span>{t("labelFilter.title", { count: selected.length })}</span>
        <span className="label-filter-summary-actions">
          {canSetMatch && (
            <span className="segment label-match-toggle">
              <button
                type="button"
                className={`segment-btn${match === "any" ? " active" : ""}`}
                aria-pressed={match === "any"}
                onClick={(e) => {
                  // Clicks inside <summary> toggle the <details> by default.
                  e.preventDefault();
                  onSetMatch("any");
                }}
                title={t("labelFilter.matchAnyHint")}
              >
                OR
              </button>
              <button
                type="button"
                className={`segment-btn${match === "all" ? " active" : ""}`}
                aria-pressed={match === "all"}
                onClick={(e) => {
                  e.preventDefault();
                  onSetMatch("all");
                }}
                title={t("labelFilter.matchAllHint")}
              >
                AND
              </button>
            </span>
          )}
          {selected.length > 0 && onClear && (
            <button
              type="button"
              className="label-filter-clear"
              onClick={(e) => {
                // Clicks inside <summary> toggle the <details> by default.
                e.preventDefault();
                setLocalOpen(false);
                onClear();
              }}
            >
              {t("labelFilter.clear")}
            </button>
          )}
        </span>
      </summary>
      <div className="label-filter-body">
        {groups.map((group) => {
          if (group.labels.length === 0) return null;
          const groupLabels = group.labels.map((l) => l.label);
          const allSelected = group.labels.every((l) => selectedSet.has(l.label));
          return (
            <section key={group.prefix} className="label-filter-section">
              <button
                type="button"
                className="label-filter-section-head"
                onClick={() => onToggleMany(groupLabels, !allSelected)}
              >
                <span className="label-filter-section-prefix">
                  {group.prefix === CATEGORY_GROUP_KEY ? t("labelPrefix.category") : group.prefix}
                </span>
                <span className="label-filter-section-count">({group.labels.length})</span>
                <span className="label-filter-section-action">
                  {allSelected ? t("labelFilter.deselectAll") : t("labelFilter.selectAll")}
                </span>
              </button>
              <div className="label-filter-chips">
                {group.labels.map((l) => {
                  const isOn = selectedSet.has(l.label);
                  return (
                    <button
                      type="button"
                      key={l.label}
                      className={`label-filter-chip${isOn ? " active" : ""}`}
                      aria-pressed={isOn}
                      onClick={() => onToggle(l.label)}
                    >
                      <span>{l.label}</span>
                      {l.count !== undefined && (
                        <span className="label-filter-chip-count">({l.count})</span>
                      )}
                    </button>
                  );
                })}
              </div>
            </section>
          );
        })}
      </div>
    </details>
  );
}
