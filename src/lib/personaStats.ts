import type { PersonalityProfileContent } from "@/types/api";
import { formatProfileContent } from "./personalitySignal";

/**
 * Persona-stats header values.
 *
 * `categories` is the count of the spec's 6 categories (interests /
 * preferences / decision_style / communication_style / values_and_beliefs /
 * anti_preferences) that actually carry content in the merged profile; the
 * UI shows it as `<categories> / PERSONA_CATEGORIES`.
 *
 * `threads` and `signals` come from the caller
 * (`PersonalityResponse.thread_count` / `.signal_count`) so they reflect
 * server-side counts independent of the profile's JSON parse. `signals` is
 * the number of `personality_signal`-tagged threads — i.e. the evidence the
 * merged profile was actually built from — which equals the row count of
 * the signal drawer, so the badge and the drawer never disagree.
 */
export interface PersonaStats {
  threads: number;
  signals: number;
  categories: number;
  profile_version: string;
}

export const PERSONA_CATEGORIES = 6 as const;

/**
 * Count how many of the 6 profile categories carry displayable content. Derived
 * from `formatProfileContent` (the grid's own renderer) so the badge can never
 * disagree with the grid — a category that flattens to nothing (e.g. a style
 * object with only blank fields) is dropped by both.
 */
export function countPopulatedCategories(content: PersonalityProfileContent | null): number {
  if (!content) return 0;
  return formatProfileContent(content).length;
}

/**
 * Build the full PersonaStats shape from precomputed server counts and the
 * parsed profile content. Safe to call with `null` profile.
 */
export function buildPersonaStats(args: {
  threads: number;
  signals: number;
  content: PersonalityProfileContent | null;
}): PersonaStats {
  return {
    threads: args.threads,
    signals: args.signals,
    categories: countPopulatedCategories(args.content),
    profile_version: args.content?.profile_version?.trim() || "-",
  };
}
