import { describe, expect, it } from "vitest";
import workflow from "../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml?raw";
import { blockBetween, outputExpression, runStep } from "./lib/workflowJq";

const policy = (resolution_mode: string, cross_thread_resolution_mode: string) =>
  JSON.stringify({ resolution_mode, cross_thread_resolution_mode });

describe("cross-thread resolution dispatch from a thread summary", () => {
  const dispatches = (overrides: Record<string, unknown> = {}, summary_json: unknown = { title: "t" }) =>
    runStep(workflow, "dispatchCrossThreadResolution", "if", {
      summary_json,
      workflow: {
        input: {
          issue_tracker_grpc_host: "127.0.0.1",
          policy_json: policy("APPLY", "APPLY"),
          ...overrides,
        },
      },
    });

  it("reserves a run only after a saved summary with both switches on and a tracker", () => {
    expect(dispatches()).toBe(true);
    expect(dispatches({}, null)).toBe(false);
    expect(dispatches({ policy_json: policy("APPLY", "DISABLED") })).toBe(false);
    expect(dispatches({ policy_json: policy("DISABLED", "APPLY") })).toBe(false);
    expect(dispatches({ policy_json: "" })).toBe(false);
    expect(dispatches({ issue_tracker_grpc_host: "" })).toBe(false);
  });

  it("reserves after the candidates of this summary were extracted, without waiting", () => {
    expect(workflow.indexOf("- dispatchCrossThreadResolution:")).toBeGreaterThan(workflow.indexOf("- extractIssues:"));
    expect(workflow.indexOf("- dispatchCrossThreadResolution:")).toBeLessThan(workflow.lastIndexOf("\noutput:"));
    const dispatch = blockBetween(workflow, "- dispatchCrossThreadResolution:", "\noutput:");
    expect(dispatch).toContain("await: false");
    expect(dispatch).toContain("workerName: lookback-cross-thread-issue-resolution\n");
  });

  it("starts an automatic run for the summary's user", () => {
    const input = JSON.parse(
      runStep(workflow, "invokeCrossThreadResolution", "input", {
        workflow: {
          input: {
            user_id: 7,
            memories_grpc_host: "m",
            memories_grpc_port: 1,
            issue_tracker_grpc_host: "t",
            issue_tracker_grpc_port: 2,
            policy_json: "{}",
          },
        },
      }) as string,
    );
    expect(input).toMatchObject({ mode: "auto", user_id: 7, memories_grpc_host: "m", issue_tracker_grpc_port: 2 });
    expect(outputExpression(workflow, "cross_thread_resolution_dispatched")).toContain("cross_thread_resolution_dispatched");
  });
});
