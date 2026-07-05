import type { QueryClient } from "@tanstack/react-query";
import { refreshGeneratedCaches } from "@/lib/generatedRefresh";
import type { GeneratedRefreshEvent } from "@/types/api";
import { useTauriEvent } from "./useTauriEvent";

export function useGeneratedRefresh(queryClient: QueryClient): void {
  useTauriEvent<GeneratedRefreshEvent>("generated://refresh", (event) => {
    refreshGeneratedCaches(queryClient, event.scopes);
  });
}
