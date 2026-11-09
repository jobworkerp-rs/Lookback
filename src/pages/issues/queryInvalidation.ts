import type { QueryClient } from "@tanstack/react-query";

type IssueQueryScope = "review" | "inventory" | "all";

const queryKeysByScope: Record<IssueQueryScope, readonly (readonly string[])[]> = {
  review: [["issue-review"]],
  inventory: [["issues"], ["issue-history"]],
  all: [["issue-review"], ["issues"], ["issue-history"]],
};

/** Invalidates only the data sets changed by an Issue-management mutation. */
export function invalidateIssueQueries(queryClient: QueryClient, scope: IssueQueryScope) {
  return Promise.all(
    queryKeysByScope[scope].map((queryKey) => queryClient.invalidateQueries({ queryKey })),
  );
}
