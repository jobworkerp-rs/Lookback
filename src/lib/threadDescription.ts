// Summary workflows write `ThreadData.description` as
// `【<title>】 <summary markdown>` (see
// `workers/workflows/thread-summary/thread-summary-single.yaml` step
// `updateThreadDescription`). The UI wants the short title for headers
// and the (long, markdown) body folded away separately, so split the two
// here rather than rendering the whole blob as a plain-text title.

export interface ParsedThreadDescription {
  /** Short heading: the `【...】` title, or the whole string when unbracketed. */
  title: string;
  /** Markdown summary body after the title, or null when there is none. */
  body: string | null;
}

const TITLE_OPEN = "【";
const TITLE_CLOSE = "】";

/**
 * Split a thread description into a short title and an optional markdown body.
 *
 * - `【T】 B`  -> { title: "T", body: "B" }
 * - `【T】`    -> { title: "T", body: null }  (no trailing content)
 * - `plain`    -> { title: "plain", body: null }  (un-summarized threads)
 * - null/""    -> { title: fallback, body: null }
 *
 * The close bracket may be followed by an optional single space before the
 * body; both the YAML-produced "】 " and a stray "】" are handled.
 */
export function parseThreadDescription(
  description: string | null | undefined,
  fallback: string,
): ParsedThreadDescription {
  const text = description?.trim();
  if (!text) return { title: fallback, body: null };

  if (text.startsWith(TITLE_OPEN)) {
    const close = text.indexOf(TITLE_CLOSE);
    if (close > 0) {
      const title = text.slice(TITLE_OPEN.length, close).trim();
      const body = text.slice(close + TITLE_CLOSE.length).trim();
      return {
        title: title || fallback,
        body: body.length > 0 ? body : null,
      };
    }
  }

  return { title: text, body: null };
}
