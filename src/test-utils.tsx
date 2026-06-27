import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { type RenderResult, render } from "@testing-library/react";
import type { ReactElement } from "react";
import { I18nextProvider } from "react-i18next";
import i18n from "@/i18n";

/**
 * Render a component tree with a fresh `QueryClient` so each test gets an
 * isolated cache. Retry is disabled so tests don't hang waiting for the
 * useQuery's exponential backoff before a mocked rejection lands.
 *
 * Tests that need to spy on the client directly (e.g. `invalidateQueries`)
 * should keep instantiating their own client and providing it inline —
 * this helper only covers the common "just wrap and render" case.
 */
export function renderWithQueryClient(node: ReactElement): RenderResult {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(<QueryClientProvider client={client}>{node}</QueryClientProvider>);
}

/**
 * Render with both the i18n provider and a fresh QueryClient. Use for any tree
 * that calls `useTranslation`. The shared i18n singleton keeps whatever language
 * was last set, so language-sensitive tests should set it explicitly.
 */
export function renderWithProviders(node: ReactElement): RenderResult {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>{node}</QueryClientProvider>
    </I18nextProvider>,
  );
}
