import { describe, expect, it, vi } from "vitest";
import { invalidateIssueQueries } from "./queryInvalidation";

function fakeQueryClient() {
  return {
    invalidateQueries: vi.fn().mockResolvedValue(undefined),
  };
}

describe("invalidateIssueQueries", () => {
  it("invalidates every list changed by a resolution", async () => {
    const queryClient = fakeQueryClient();

    await invalidateIssueQueries(queryClient as never, "all");

    expect(queryClient.invalidateQueries).toHaveBeenCalledTimes(3);
    expect(queryClient.invalidateQueries).toHaveBeenCalledWith({ queryKey: ["issue-review"] });
    expect(queryClient.invalidateQueries).toHaveBeenCalledWith({ queryKey: ["issues"] });
    expect(queryClient.invalidateQueries).toHaveBeenCalledWith({ queryKey: ["issue-history"] });
  });

  it("keeps candidate editing scoped to the review list", async () => {
    const queryClient = fakeQueryClient();

    await invalidateIssueQueries(queryClient as never, "review");

    expect(queryClient.invalidateQueries).toHaveBeenCalledTimes(1);
    expect(queryClient.invalidateQueries).toHaveBeenCalledWith({ queryKey: ["issue-review"] });
  });
});
