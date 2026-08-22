import type { SummaryValue } from "@/types/api";

export type SummaryObject = Record<string, SummaryValue>;

export interface SummarySelectionExcerpt {
  title: string | null;
  summary: string | null;
  category: string | null;
  status: string | null;
  decisions: string[];
}

export function asSummaryObject(value: unknown): SummaryObject | null {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as SummaryObject)
    : null;
}

export function summaryStrings(value: SummaryValue | undefined): string[] {
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === "string" && item.trim() !== "")
    : [];
}

export function summaryText(value: SummaryValue | undefined): string | null {
  return typeof value === "string" && value.trim() !== "" ? value : null;
}

export function parseSummarySelection(raw: string): SummaryObject | null {
  try {
    return asSummaryObject(JSON.parse(raw));
  } catch {
    return null;
  }
}

export function summarySelectionExcerpt(raw: string): SummarySelectionExcerpt {
  return summarySelectionExcerptFromParsed(parseSummarySelection(raw), raw);
}

export function summarySelectionExcerptFromParsed(
  parsed: SummaryObject | null,
  raw: string,
): SummarySelectionExcerpt {
  if (!parsed) {
    return {
      title: null,
      summary: raw.trim().slice(0, 240) || null,
      category: null,
      status: null,
      decisions: [],
    };
  }
  return {
    title: summaryText(parsed.title) ?? summaryText(parsed.タイトル) ?? summaryText(parsed.件名),
    summary: summaryText(parsed.summary) ?? summaryText(parsed.overall_purpose),
    category: summaryText(parsed.category),
    status: summaryText(parsed.status),
    decisions: summaryStrings(parsed.key_decisions).slice(0, 2),
  };
}
