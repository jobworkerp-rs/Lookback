import { createContext, useContext } from "react";

/**
 * App-wide display timezone (an IANA name, e.g. `"Asia/Tokyo"`), for passing to
 * `formatDateTime` alongside the locale tag from {@link useLocaleTag}.
 *
 * Provided once at the App root (which reads the backend-resolved
 * `effective_timezone` from the `["app-settings"]` query) and consumed by the
 * many leaf timestamp views. A Context — NOT a per-view `useQuery` — so those
 * leaves stay QueryClient-agnostic (a timestamp span shouldn't need a
 * QueryClientProvider in its unit test); `undefined` (no provider, or before
 * the query resolves) makes `formatDateTime` fall back to the host zone.
 */
export const TimezoneContext = createContext<string | undefined>(undefined);

export function useTimezone(): string | undefined {
  return useContext(TimezoneContext);
}
