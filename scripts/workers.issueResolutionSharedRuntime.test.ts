import { describe, expect, it } from "vitest";
import applyWorkflow from "../workers/workflows/issue-resolution-shared/apply.yaml?raw";
import judgeWorkflow from "../workers/workflows/issue-resolution-shared/judge.yaml?raw";
import sameThreadWorkflow from "../workers/workflows/thread-issue-resolution/thread-issue-resolution.yaml?raw";
import prepareWorkflow from "../workers/workflows/issue-resolution-shared/prepare.yaml?raw";
import groupWorkflow from "../workers/workflows/thread-group-issue-resolution/thread-group-issue-resolution-batch.yaml?raw";
import extractWorkflow from "../workers/workflows/issue-extract/issue-extract.yaml?raw";
import { blockBetween, runStep, stepExpression } from "./lib/workflowJq";

describe("shared prepare protections", () => {
  const pending = (resolution_state: string) =>
    runStep(prepareWorkflow, "protectPendingResolution", "if", { candidate_outcome_recorded: false, resolution_state });

  it("protects a candidate whose earlier resolution is still pending", () => {
    expect(pending("REVIEW_CANDIDATE_RESOLUTION_STATE_RESOLUTION_PENDING")).toBe(true);
    expect(pending("RESOLUTION_PENDING")).toBe(true);
    expect(pending("REVIEW_CANDIDATE_RESOLUTION_STATE_ACTIONABLE")).toBe(false);
    expect(
      runStep(prepareWorkflow, "protectPendingResolution", "protected_candidates", {
        protected_candidates: [],
        candidate_id: "c1",
      }),
    ).toEqual([{ candidate_id: "c1", reason: "existing_resolution_pending" }]);
  });

  it("protects a candidate whose Issue is claimed", () => {
    const claimed = (associated_issue_record: unknown) =>
      runStep(prepareWorkflow, "protectActiveClaim", "if", { candidate_outcome_recorded: false, associated_issue_record });
    expect(claimed({ claim: { leaseOwner: "agent" } })).toBe(true);
    expect(claimed({ claim: null })).toBe(false);
    expect(claimed({})).toBe(false);
  });

  it("fails closed when the associated Issue cannot be read", () => {
    expect(
      runStep(prepareWorkflow, "recordAssociatedIssueFailure", "failed_candidates", {
        failed_candidates: [],
        candidate_id: "c1",
        associated_issue_fetch_error: "unavailable",
      }),
    ).toEqual([{ candidate_id: "c1", reason: "unavailable", source: "GetIssueWithSync" }]);
    expect(
      runStep(prepareWorkflow, "recordAssociatedIssueFailure", "candidate_outcome_recorded", {}),
    ).toBe(true);
  });
});

describe("shared judge", () => {
  const message = (id: string, createdAt: number | null) => ({
    id: { value: id },
    data: { role: "ROLE_USER", content: `text ${id}`, createdAt },
  });
  const rawMessages = (max_message_at_by_thread: Record<string, number>) =>
    (
      runStep(
        judgeWorkflow,
        "findRawMemberMessages",
        "selected_thread_raw_messages",
        { selected_thread_id: "t1", max_message_at_by_thread, messages_after: null },
        { jsonBody: JSON.stringify([message("a", 100), message("b", 200), message("c", 300), message("d", null)]) },
      ) as { id: string }[]
    ).map((m) => m.id);

  it("reads messages up to the thread's checkpoint when one is given", () => {
    expect(rawMessages({ t1: 200 })).toEqual(["a", "b"]);
    expect(rawMessages({ other: 200 })).toEqual(["a", "b", "c", "d"]);
    expect(rawMessages({})).toEqual(["a", "b", "c", "d"]);
  });

  it("reads only messages strictly after the given time when one is given", () => {
    const after = (messages_after: number | null) =>
      (
        runStep(
          judgeWorkflow,
          "findRawMemberMessages",
          "selected_thread_raw_messages",
          { selected_thread_id: "t1", max_message_at_by_thread: {}, messages_after },
          { jsonBody: JSON.stringify([message("a", 100), message("b", 200), message("d", null)]) },
        ) as { id: string }[]
      ).map((m) => m.id);
    expect(after(100)).toEqual(["b"]);
    expect(after(null)).toEqual(["a", "b", "d"]);
  });

  it("reports when the input window dropped older messages", () => {
    const truncated = (window: number, all: number) =>
      runStep(judgeWorkflow, "indexRawInputWindow", "raw_input_truncated", {
        raw_input_window: Array.from({ length: window }, (_, i) => ({ id: `${i}` })),
        raw_member_messages: Array.from({ length: all }, (_, i) => ({ id: `${i}` })),
      });
    expect(truncated(2, 3)).toBe(true);
    expect(truncated(3, 3)).toBe(false);
  });

  it("marks only fetch and LLM call failures as transient", () => {
    expect(runStep(judgeWorkflow, "failOnRawFetchError", "transient_failure", {})).toBe(true);
    expect(runStep(judgeWorkflow, "recordResolutionLlmFailure", "transient_failure", {})).toBe(true);
    expect(blockBetween(judgeWorkflow, "- failOnInvalidLlmResponse:", "- skipNoResolution:")).not.toContain(
      "transient_failure",
    );
  });

  it("keeps the length and both ends of an invalid LLM response", () => {
    const text = `{"decision": "COMPLETE", "rationale": "${"x".repeat(500)}`;
    const raw_input_window = [{ content: "abc" }, { content: "de" }];
    const response = runStep(judgeWorkflow, "shapeResolution", "llm_response", {
      llm_text: text,
      llm_json: null,
      raw_input_window,
    });
    expect(response).toEqual({
      length: text.length,
      head: text.slice(0, 200),
      tail: text.slice(-200),
      input_messages: 2,
      input_chars: 5,
    });
    expect(
      runStep(judgeWorkflow, "shapeResolution", "llm_response", { llm_text: "", llm_json: null, raw_input_window: [] }),
    ).toEqual({ length: 0, head: "", tail: "", input_messages: 0, input_chars: 0 });
    expect(
      runStep(judgeWorkflow, "failOnInvalidLlmResponse", "failed_candidates", {
        failed_candidates: [],
        candidate_id: "c1",
        llm_response: response,
      }),
    ).toEqual([
      { candidate_id: "c1", reason: "invalid resolution LLM response", source: "resolution_llm", llm_response: response },
    ]);
  });

  it("accepts evidence only from the threads it was asked to read", () => {
    const differentMember = (selected_member_thread_ids: string[]) =>
      runStep(judgeWorkflow, "validateCrossThreadEvidence", "different_member", {
        evidence_messages: [{ thread_id: "t2" }],
        source_thread_ids: ["t1"],
        selected_member_thread_ids,
      });
    expect(differentMember(["t2"])).toBe(true);
    expect(differentMember(["t3"])).toBe(false);
  });

  it("names the evidence scope given by the caller in the prompt", () => {
    const prompt = blockBetween(judgeWorkflow, "- runResolutionLlm:", "options:");
    expect(prompt).toContain("{{ evidence_scope }}");
    expect(groupWorkflow).toContain(
      'evidence_scope: "active Thread Group members other than the candidate source thread"',
    );
  });
});

describe("group merge of shared worker results", () => {
  const lists = {
    skipped_candidate_ids: ["old"],
    skipped_candidates: [{ candidate_id: "old", reason: "x" }],
    protected_candidates: [],
    failed_candidates: [],
  };

  it("appends a worker's outcome lists and adopts whether it recorded the candidate", () => {
    const prepared = {
      candidate_outcome_recorded: true,
      skipped_candidate_ids: [],
      skipped_candidates: [],
      protected_candidates: [{ candidate_id: "c1", reason: "active_claim" }],
      failed_candidates: [],
      candidate_id: "c1",
    };
    const bindings = { ...lists, prepared };
    expect(runStep(groupWorkflow, "adoptPreparedCandidate", "protected_candidates", bindings)).toEqual(
      prepared.protected_candidates,
    );
    expect(runStep(groupWorkflow, "adoptPreparedCandidate", "skipped_candidates", bindings)).toEqual(
      lists.skipped_candidates,
    );
    expect(runStep(groupWorkflow, "adoptPreparedCandidate", "candidate_outcome_recorded", bindings)).toBe(true);
    expect(runStep(groupWorkflow, "adoptPreparedCandidate", "candidate_id", bindings)).toBe("c1");
  });

  it("records a failed worker call against the candidate", () => {
    expect(
      runStep(groupWorkflow, "recordJudgeFailure", "failed_candidates", {
        failed_candidates: [],
        candidate: { candidateId: "c1" },
        judge_error: "timeout",
      }),
    ).toEqual([{ candidate_id: "c1", reason: "timeout", source: "judge" }]);
  });

  it("continues to recordApplied only for a committed operation", () => {
    const outcome = (applied: Record<string, unknown>) => ({
      recorded: runStep(groupWorkflow, "adoptApplyOutcome", "candidate_outcome_recorded", { applied }),
      payload: runStep(groupWorkflow, "adoptApplyOutcome", "decision_payload", {
        applied,
        decision_payload: { reject: {} },
      }),
    });
    expect(outcome({ operation_confirmed: true, conflicted_candidates: [], failed_candidates: [] })).toEqual({
      recorded: false,
      payload: { reject: {} },
    });
    expect(
      outcome({ operation_confirmed: false, conflicted_candidates: [{ candidate_id: "c1" }], failed_candidates: [] }),
    ).toEqual({ recorded: true, payload: null });
  });
});

describe("shared apply input", () => {
  it("accepts the tracker's uint64 revision, which JSON carries as a string", () => {
    expect(applyWorkflow).toMatch(/candidate_revision: \{ type: \["integer", "string"\]/);
  });
});

describe("ResolveCandidate request shape", () => {
  // `decision` is the proto oneof's name; protobuf JSON puts its member
  // (`reject`, `confirm_associated_issue`, ...) directly in the request, and
  // a `decision` key is an unknown field the server never sees.
  const decision_payload = { reject: { reason_category: "RESOLUTION_REASON_CATEGORY_NOT_ACTIONABLE", rationale: "r" } };

  it("puts the decision member at the top level from the shared apply", () => {
    const body = JSON.parse(
      runStep(applyWorkflow, "invokeResolveCandidate", "json_body", {
        candidate_id: "c1",
        candidate_revision: "2",
        decision_payload,
        manager_contract: {},
        manager_store_identity: {},
        workflow: { input: { idempotency_key: "k", actor_id: "a", audit_reason: "r", correlation_prefix: "p" } },
      }) as string,
    );
    expect(body).not.toHaveProperty("decision");
    expect(body.reject).toEqual(decision_payload.reject);
    expect(body.expected_revision).toBe("2");
  });

  it("puts the decision member at the top level from the same-thread resolution", () => {
    const body = runStep(sameThreadWorkflow, "prepareResolveCandidateRequest", "resolution_request_payload", {
      candidate_id: "c1",
      candidate_revision: "2",
      decision_payload,
      manager_contract: {},
      manager_store_identity: {},
      decision_key: "k",
      decision_audit_rationale: "r",
      evidence_memory_ids: ["m1"],
    }) as Record<string, unknown>;
    expect(body).not.toHaveProperty("decision");
    expect(body.reject).toEqual(decision_payload.reject);
  });
});

const firstFailure = (workflow: string, step: string, extra: Record<string, unknown>) =>
  (
    runStep(workflow, step, "failed_candidates", { failed_candidates: [], candidate_id: "c1", ...extra }) as {
      permanent?: boolean;
    }[]
  )[0];

describe("shared prepare failures", () => {
  it("marks an operation history longer than one page as permanent", () => {
    const historyPage = { type: "https://lookback.local/errors/issue-resolution/history-page" };
    for (const [step, key] of [
      ["recordHistoryFailure", "history_error_value"],
      ["recordAssociatedIssueHistoryFailure", "issue_history_error_value"],
    ]) {
      expect(firstFailure(prepareWorkflow, step, { [key]: historyPage }).permanent).toBe(true);
      expect(firstFailure(prepareWorkflow, step, { [key]: "timeout" }).permanent).toBe(false);
    }
  });
});

describe("shared apply confirmation", () => {
  it("keeps the tracker's error when ResolveCandidate returns no receipt", () => {
    const text = runStep(applyWorkflow, "describeApplyFailure", "apply_failure_text", {
      apply_error: { title: "ResolveCandidate did not return a receipt" },
      resolution_response: { error: { code: "MANAGER_ERROR_CODE_REVISION_CONFLICT" } },
    }) as string;
    expect(text).toContain("did not return a receipt");
    expect(text).toContain("REVISION_CONFLICT");
    // A transport failure leaves no response to quote.
    expect(
      runStep(applyWorkflow, "describeApplyFailure", "apply_failure_text", {
        apply_error: "timeout",
        resolution_response: null,
      }),
    ).toBe('"timeout"');
  });

  it("marks a tracker rejection a rerun cannot change as permanent", () => {
    const failure = (resolution_response: unknown) =>
      firstFailure(applyWorkflow, "recordApplyFailure", { apply_failure_text: "rejected", resolution_response });
    for (const code of ["PRECONDITION_FAILED", "CANDIDATE_NOT_FOUND", "MANAGER_ERROR_CODE_INVALID_STATE_TRANSITION"]) {
      expect(failure({ error: { code } }).permanent).toBe(true);
    }
    expect(failure({ error: { errorCode: "PRECONDITION_FAILED" } }).permanent).toBe(true);
    for (const response of [{ error: { code: "BACKEND_UNAVAILABLE" } }, { error: { code: "RESULT_UNCONFIRMED" } }, null]) {
      expect(failure(response).permanent).toBe(false);
    }
  });

  it("reads the operation until the tracker has committed it", () => {
    // The tracker commits through an outbox, so the first read after the
    // receipt is often still pending.
    const poll = blockBetween(applyWorkflow, "- pollAppliedOperation:", "- requireCommittedOperation:");
    expect(poll).toContain("[range(0; 10)]");
    expect(poll).toContain("milliseconds: 500");
    const committed = (operation_state: string) =>
      runStep(applyWorkflow, "requireCommittedOperation", "if", { operation_state });
    expect(committed("DURABLE_OPERATION_STATE_COMMITTED")).toBe(false);
    expect(committed("DURABLE_OPERATION_STATE_PENDING")).toBe(true);
  });
});

describe("audit reason length", () => {
  // The tracker rejects an actor_audit.reason over 1024 characters.
  const ids = Array.from({ length: 80 }, (_, i) => `75000000000000${String(i).padStart(5, "0")}`);
  const reason = `Cross-thread conversation evidence explicitly establishes completion. [evidence_thread_ids=t1,t2] [evidence_memory_ids=${ids.join(",")}]`;

  it("keeps every thread ID and as many memory IDs as fit in the shared apply", () => {
    const body = JSON.parse(
      runStep(applyWorkflow, "invokeResolveCandidate", "json_body", {
        candidate_id: "c1",
        candidate_revision: "2",
        decision_payload: { reject: {} },
        manager_contract: {},
        manager_store_identity: {},
        workflow: { input: { idempotency_key: "k", actor_id: "a", audit_reason: reason, correlation_prefix: "p" } },
      }) as string,
    );
    const sent = body.context.actor_audit.reason as string;
    expect(sent.length).toBeLessThanOrEqual(1024);
    expect(sent).toContain("[evidence_thread_ids=t1,t2]");
    expect(sent).toMatch(/,\+\d+ more\]$/);
    expect(sent).toContain(ids[0]);
    const short = "fixed [evidence_thread_ids=t1] [evidence_memory_ids=m1]";
    expect(
      JSON.parse(
        runStep(applyWorkflow, "invokeResolveCandidate", "json_body", {
          candidate_id: "c1",
          candidate_revision: "2",
          decision_payload: { reject: {} },
          manager_contract: {},
          manager_store_identity: {},
          workflow: { input: { idempotency_key: "k", actor_id: "a", audit_reason: short, correlation_prefix: "p" } },
        }) as string,
      ).context.actor_audit.reason,
    ).toBe(short);
  });

  it("fits the same-thread resolution's reason the same way", () => {
    const definition = (source: string, step: string, key: string) => {
      const expression = stepExpression(source, step, key);
      const start = expression.indexOf("def fit_audit_reason");
      expect(start).toBeGreaterThan(-1);
      return expression.slice(start, expression.indexOf("end;", start)).replace(/\s+/g, " ");
    };
    expect(definition(sameThreadWorkflow, "prepareResolveCandidateRequest", "resolution_request_payload")).toBe(
      definition(applyWorkflow, "invokeResolveCandidate", "json_body"),
    );
    const body = runStep(sameThreadWorkflow, "prepareResolveCandidateRequest", "resolution_request_payload", {
      candidate_id: "c1",
      candidate_revision: "2",
      decision_payload: { reject: {} },
      manager_contract: {},
      manager_store_identity: {},
      decision_key: "k",
      decision_audit_rationale: "r",
      evidence_memory_ids: ids,
    }) as { context: { actor_audit: { reason: string } } };
    expect(body.context.actor_audit.reason.length).toBeLessThanOrEqual(1024);
  });
});

describe("LLM output token floors", () => {
  it("leaves a reasoning model room to finish the JSON answer", () => {
    // A reasoning model spends output tokens on its reasoning first; at 2000
    // the answer was cut off and the server returned an empty text. The
    // extraction (4000) and the group's thread selection (1000) hit the same.
    for (const [document, floor] of [
      [judgeWorkflow, 16000],
      [sameThreadWorkflow, 16000],
      [extractWorkflow, 16000],
      [groupWorkflow, 10000],
    ] as const) {
      const limits = [...document.matchAll(/max_tokens: (\d+)/g)].map((m) => Number(m[1]));
      expect(limits.length).toBeGreaterThan(0);
      expect(Math.min(...limits)).toBeGreaterThanOrEqual(floor);
    }
  });
});
