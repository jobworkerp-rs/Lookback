import i18n from "@/i18n";

/**
 * Coerce a caught value into a displayable message.
 *
 * Tauri's `invoke` rejects with whatever the Rust command's error serializes
 * to. `AppError` serializes via `serialize_str(self.to_string())`, so a failed
 * command throws a plain **string**, not an `Error` — `(e as Error).message`
 * would be `undefined`. Handle strings, `Error` instances, and anything else
 * so a failure always surfaces a reason to the user.
 */
// Dotted msg_ids emitted by the backend (e.g. "periodic.validation.nameRequired").
// Legacy free-form messages never match and pass through untouched; the guard
// also avoids i18next's ":" namespace-splitting edge cases on arbitrary text.
const MSG_ID_PATTERN = /^[a-z][a-zA-Z0-9]*(\.[a-zA-Z0-9]+)+$/;

export function errorMessage(e: unknown): string {
  const raw = typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
  if (!MSG_ID_PATTERN.test(raw)) return raw;
  const translated = i18n.t(raw);
  return typeof translated === "string" && translated !== raw ? translated : raw;
}
