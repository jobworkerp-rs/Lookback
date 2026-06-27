/**
 * Coerce a caught value into a displayable message.
 *
 * Tauri's `invoke` rejects with whatever the Rust command's error serializes
 * to. `AppError` serializes via `serialize_str(self.to_string())`, so a failed
 * command throws a plain **string**, not an `Error` — `(e as Error).message`
 * would be `undefined`. Handle strings, `Error` instances, and anything else
 * so a failure always surfaces a reason to the user.
 */
export function errorMessage(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return String(e);
}
