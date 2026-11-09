import { describe, expect, it } from "vitest";
import candidateWorkflow from "../workers/workflows/cross-thread-issue-resolution/cross-thread-candidate.yaml?raw";
import workflow from "../workers/workflows/cross-thread-issue-resolution/cross-thread-issue-resolution.yaml?raw";
import groupWorkflow from "../workers/workflows/thread-group-issue-resolution/thread-group-issue-resolution-batch.yaml?raw";
import prepareWorkflow from "../workers/workflows/issue-resolution-shared/prepare.yaml?raw";
import extractWorkers from "../workers/lookback-issue-extract-workers.yaml?raw";
import migrateScript from "./migrate-repo-labels.sh?raw";
import { blockBetween, evaluate, outputExpression, runStep, stepExpression } from "./lib/workflowJq";

const lookback = "repo:git.example.com/jobworkerp-rs/Lookback-app";
const snapshot = (labels: string[]) => [
  {
    key: "lookback.source_label_snapshots_json",
    value: JSON.stringify([{ thread_id: "s1", extraction_run_id: "run-1", labels }]),
  },
  { key: "unrelated", value: "x".repeat(10) },
];
const row = (id: string, labels: string[] | null, updatedAt: number, sources = ["s1"]) => ({
  candidate: {
    candidateId: id,
    title: `title ${id}`,
    description: `description ${id}`,
    metadata: labels == null ? [] : snapshot(labels),
    sourceEvidence: sources.map((threadId) => ({ threadId, memoryId: `m-${threadId}` })),
    updatedAt: { unixEpochMillis: `${updatedAt}` },
  },
  integrity: "NORMAL",
  resolutionState: "ACTIONABLE",
});

describe("cross-thread run range", () => {
  const since = ({ mode, ...input }: Record<string, unknown>, last_watermark_at: number | null) =>
    runStep(workflow, "computeSince", "since_ms", {
      mode,
      workflow: { input: { clock_skew_ms: 300000, initial_lookback_ms: 1000000, ...input } },
      last_watermark_at,
      run_started_at: 10000000,
    });

  it("continues from the last successful automatic run minus the clock skew", () => {
    expect(since({ mode: "auto" }, 5000000)).toBe(4700000);
  });

  it("looks back a fixed period when no run has succeeded yet", () => {
    expect(since({ mode: "auto" }, null)).toBe(9000000);
  });

  it("lets a manual run choose its own start and ignore the automatic one", () => {
    expect(since({ mode: "manual", since_ms: 123 }, 5000000)).toBe(123);
    expect(since({ mode: "manual" }, 5000000)).toBe(0);
  });

  it("reads the start of the last successful run from the watermark job's key", () => {
    const results = [{ id: { value: "9" }, data: { uniqKey: "lookback-cross-thread-watermark:4242", status: "SUCCESS" } }];
    expect(runStep(workflow, "findLastWatermark", "last_watermark_at", {}, { jsonBody: JSON.stringify(results) })).toBe(
      4242,
    );
    expect(runStep(workflow, "findLastWatermark", "last_watermark_at", {}, { jsonBody: "[]" })).toBeNull();
    expect(
      runStep(workflow, "findLastWatermark", "last_watermark_at", {}, { jsonBody: JSON.stringify([{ data: {} }]) }),
    ).toBeNull();
  });

  it("records a watermark only after an automatic run without transient failures", () => {
    const writes = (mode: string, failed_candidates: unknown[], dry_run = false) =>
      runStep(workflow, "writeWatermark", "if", { mode, dry_run, failed_candidates });
    expect(writes("auto", [])).toBe(true);
    expect(writes("auto", [{ candidate_id: "c1" }])).toBe(false);
    // A failure a rerun cannot fix must not hold every later run back.
    expect(writes("auto", [{ candidate_id: "c1", permanent: true }])).toBe(true);
    expect(writes("auto", [{ candidate_id: "c1", permanent: true }, { candidate_id: "c2" }])).toBe(false);
    expect(writes("manual", [])).toBe(false);
    expect(writes("auto", [], true)).toBe(false);
    const body = JSON.parse(
      runStep(workflow, "enqueueWatermark", "json_body", { run_started_at: 4242 }) as string,
    );
    expect(body.uniq_key).toBe("lookback-cross-thread-watermark:4242");
    expect(body.worker_name).toBe("lookback-cross-thread-issue-resolution-watermark");
  });

  it("is enabled automatically only when both resolution switches are on", () => {
    const disabled = (mode: string, policy: Record<string, string>, dry_run = false) =>
      runStep(workflow, "skipIfDisabled", "if", {
        mode,
        dry_run,
        resolution_mode: policy.resolution ?? "DISABLED",
        cross_thread_resolution_mode: policy.cross ?? "DISABLED",
      });
    expect(disabled("auto", { resolution: "APPLY", cross: "APPLY" })).toBe(false);
    expect(disabled("auto", { resolution: "APPLY" })).toBe(true);
    expect(disabled("auto", { cross: "APPLY" })).toBe(true);
    // An operator evaluates the backlog before switching it on.
    expect(disabled("manual", {}, true)).toBe(false);
    expect(disabled("manual", {})).toBe(true);
    expect(disabled("manual", { resolution: "APPLY" })).toBe(false);
  });
});

describe("cross-thread candidate scope", () => {
  const scope = (rows: unknown[], updated_threads: unknown[], since_ms = 1000) =>
    runStep(workflow, "scopeCandidates", "scoped_rows", {
      review_listing: { review_rows: rows },
      updated_threads,
      since_ms,
      workflow: { input: {} },
    }) as {
      review_row: { candidate: { candidateId: string; metadata: { key: string }[] } };
      candidate_side: boolean;
      updated_thread_ids: string[];
      candidate_keys: { repos: string[]; paths: string[] };
    }[];

  it("takes candidates updated since the start of the run range", () => {
    const scoped = scope([row("new", [lookback], 1000), row("old", [lookback], 999)], []);
    expect(scoped.map((s) => s.review_row.candidate.candidateId)).toEqual(["new"]);
    expect(scoped[0].candidate_side).toBe(true);
    expect(scoped[0].updated_thread_ids).toEqual([]);
  });

  it("takes older candidates whose project matches an updated thread", () => {
    const scoped = scope(
      [row("legacy", ["repo:git@git.example.com:jobworkerp-rs/Lookback-app.git"], 1), row("other", ["repo:x.org/a/b"], 1)],
      [
        { thread_id: "t2", labels: [lookback] },
        { thread_id: "s1", labels: [lookback] },
      ],
    );
    expect(scoped.map((s) => s.review_row.candidate.candidateId)).toEqual(["legacy"]);
    // The candidate's own source thread is never its evidence.
    expect(scoped[0].updated_thread_ids).toEqual(["t2"]);
    expect(scoped[0].candidate_side).toBe(false);
    expect(scoped[0].candidate_keys.repos).toEqual(["git.example.com/jobworkerp-rs/Lookback-app"]);
  });

  it("matches by path only when a side has no repo label, and never by dir", () => {
    const candidate = row("c", ["path:/w/app", "dir:app"], 1);
    expect(scope([candidate], [{ thread_id: "t2", labels: ["path:/w/app"] }])).toHaveLength(1);
    expect(scope([candidate], [{ thread_id: "t2", labels: ["dir:app"] }])).toHaveLength(0);
    expect(scope([row("r", [lookback, "path:/w/app"], 1)], [{ thread_id: "t2", labels: ["repo:x.org/a/b", "path:/w/app"] }])).toHaveLength(0);
  });

  it("narrows a manual run to the given candidates and count", () => {
    const rows = [row("a", [lookback], 5000), row("b", [lookback], 5000), row("c", [lookback], 5000)];
    const ids = (input: Record<string, unknown>) =>
      (
        runStep(workflow, "scopeCandidates", "scoped_rows", {
          review_listing: { review_rows: rows },
          updated_threads: [],
          since_ms: 1000,
          workflow: { input },
        }) as { review_row: { candidate: { candidateId: string } } }[]
      ).map((s) => s.review_row.candidate.candidateId);
    expect(ids({})).toEqual(["a", "b", "c"]);
    expect(ids({ candidate_ids: ["c", "a"] })).toEqual(["a", "c"]);
    expect(ids({ max_candidates: 2 })).toEqual(["a", "b"]);
    expect(ids({ candidate_ids: ["b", "c"], max_candidates: 1 })).toEqual(["b"]);
  });

  it("skips candidates without a usable label snapshot", () => {
    expect(scope([row("none", null, 5000)], [])).toHaveLength(0);
    expect(scope([row("agent", ["agent:codex"], 5000)], [])).toHaveLength(0);
  });

  it("keeps only the label snapshot of the candidate metadata", () => {
    const [scoped] = scope([row("new", [lookback], 1000)], []);
    expect(scoped.review_row.candidate.metadata.map((m) => m.key)).toEqual(["lookback.source_label_snapshots_json"]);
  });

  it("normalizes repository identities exactly like the Thread Group resolution", () => {
    const definition = (source: string, step: string, key: string) => {
      const expression = stepExpression(source, step, key);
      const start = expression.indexOf("def repo_identity:");
      expect(start).toBeGreaterThan(-1);
      return expression.slice(start, expression.indexOf("def ", start + 1)).replace(/\s+/g, " ");
    };
    expect(definition(workflow, "scopeCandidates", "scoped_rows")).toBe(
      definition(groupWorkflow, "validateProjectContext", "project_context"),
    );
    const scriptStart = migrateScript.indexOf("def repo_identity:");
    expect(scriptStart).toBeGreaterThan(-1);
    const scriptDefinition = migrateScript
      .slice(scriptStart, migrateScript.indexOf("# end of repo_identity", scriptStart))
      .replace(/\s+/g, " ")
      .trim();
    expect(`${scriptDefinition} `).toBe(definition(groupWorkflow, "validateProjectContext", "project_context"));
  });

  it("compares thread labels in the run and per candidate the same way", () => {
    const definition = (source: string, step: string, key: string, name: string, end: string) => {
      const expression = stepExpression(source, step, key);
      const start = expression.indexOf(`def ${name}`);
      expect(start).toBeGreaterThan(-1);
      return expression.slice(start, expression.indexOf(end, start) + end.length).replace(/\s+/g, " ");
    };
    for (const [name, end] of [
      ["thread_keys:", "};"],
      ["matches(", "end;"],
    ]) {
      expect(definition(candidateWorkflow, "selectHits", "hit_threads", name, end)).toBe(
        definition(workflow, "scopeCandidates", "scoped_rows", name, end),
      );
    }
  });

  it("parses the label snapshot exactly like the shared prepare worker", () => {
    const normalized = (expression: string) => expression.replace(/\s+/g, " ");
    const snapshotParse = (source: string, step: string, key: string) => {
      const expression = normalized(stepExpression(source, step, key));
      const start = expression.indexOf('map(select((.key // "") == "lookback.source_label_snapshots_json"))');
      return expression.slice(start, expression.indexOf("else null end", start));
    };
    expect(snapshotParse(workflow, "scopeCandidates", "scoped_rows")).toBe(
      snapshotParse(prepareWorkflow, "selectCandidateFields", "candidate_labels"),
    );
  });
});

describe("cross-thread evidence search", () => {
  const searchBody = (query_vector: number[], input: Record<string, unknown>) =>
    JSON.parse(
      runStep(candidateWorkflow, "runThreadSearch", "json_body", {
        query_vector,
        query_text: "q",
        search_labels: [lookback],
        workflow: { input: { user_id: 1, ...input } },
      }) as string,
    );

  const keys = { repos: ["git.example.com/jobworkerp-rs/Lookback-app"], paths: ["path:/w/app"] };

  it("searches with the canonical repo labels and the path labels", () => {
    expect(
      runStep(candidateWorkflow, "buildQuery", "search_labels", {
        workflow: { input: { candidate_keys: keys } },
      }),
    ).toEqual([lookback, "path:/w/app"]);
    expect(
      runStep(candidateWorkflow, "buildQuery", "query_text", {
        candidate: { title: "T", description: "D", acceptanceCriteria: ["A1", "A2"] },
      }),
    ).toBe("T\nD\nA1\nA2");
  });

  const unusable = (embedding_error: unknown) =>
    runStep(candidateWorkflow, "classifyEmbeddingError", "embedding_unusable", { embedding_error });
  const embeddingError = (kind: string) => ({ type: `https://serverlessworkflow.io/spec/1.0.0/errors/${kind}` });

  it("falls back to the full-text search only when the embedding worker cannot be used", () => {
    // A missing or misconfigured worker stays so on every run.
    for (const kind of ["configuration", "validation"]) {
      expect(unusable(embeddingError(kind))).toBe(true);
    }
    expect(runStep(candidateWorkflow, "recordEmbeddingUnavailable", "if", { embedding_unusable: true })).toBe(true);
    expect(runStep(candidateWorkflow, "recordEmbeddingFailure", "if", { embedding_unusable: true })).toBe(false);
    expect(runStep(candidateWorkflow, "recordEmbeddingUnavailable", "query_vector", {})).toEqual([]);
    expect(blockBetween(candidateWorkflow, "- recordEmbeddingUnavailable:", "- recordEmbeddingFailure:")).not.toContain(
      "failed_candidates",
    );
    const method = (query_vector: number[]) =>
      runStep(candidateWorkflow, "runThreadSearch", "method", { query_vector });
    expect(method([0.1])).toBe("/llm_memory.service.ThreadVectorService/HybridSearch");
    expect(method([])).toBe("/llm_memory.service.ThreadVectorService/SearchByText");
    const body = (query_vector: number[]) => searchBody(query_vector, { search_limit: 5 });
    expect(body([0.1]).query_vectors).toEqual([{ values: [0.1] }]);
    expect(body([])).not.toHaveProperty("query_vectors");
    expect(body([]).options.filter.labels).toEqual([lookback]);
  });

  it("retries a candidate whose embedding failed at run time", () => {
    // A degraded search could miss the evidence and let the watermark pass it.
    for (const kind of ["timeout", "runtime", "communication"]) {
      expect(unusable(embeddingError(kind))).toBe(false);
    }
    expect(unusable("boom")).toBe(false);
    expect(runStep(candidateWorkflow, "recordEmbeddingUnavailable", "if", { embedding_unusable: false })).toBe(false);
    expect(runStep(candidateWorkflow, "recordEmbeddingFailure", "if", { embedding_unusable: false })).toBe(true);
    expect(
      runStep(candidateWorkflow, "recordEmbeddingFailure", "failed_candidates", {
        failed_candidates: [],
        candidate_id: "c1",
        embedding_error: embeddingError("timeout"),
      }),
    ).toMatchObject([{ candidate_id: "c1", source: "embedding" }]);
    expect(runStep(candidateWorkflow, "recordEmbeddingFailure", "candidate_outcome_recorded", {})).toBe(true);
  });

  const hit = (id: string, labels: string[], score: number, lastMessageAt = 500) => ({
    thread: { id: { value: id }, data: { labels, lastMessageAt } },
    score,
  });
  const select = (hits: unknown[], extra: Record<string, unknown> = {}) =>
    (
      runStep(candidateWorkflow, "selectHits", "hit_threads", {
        raw_hits: runStep(candidateWorkflow, "searchThreads", "raw_hits", {}, { jsonBody: JSON.stringify(hits) }),
        source_thread_ids: ["s1"],
        workflow: { input: { candidate_keys: keys, search_limit: 2, search_score_min: 0.1, ...extra } },
      }) as { thread_id: string }[]
    ).map((h) => h.thread_id);

  it("keeps the best matching threads of the candidate's project, never its source", () => {
    expect(
      select([
        hit("s1", [lookback], 0.9),
        hit("t1", [lookback], 0.5),
        hit("t2", ["repo:x.org/a/b"], 0.8),
        hit("t3", [lookback], 0.7),
        hit("t4", [lookback], 0.6),
        hit("t5", [lookback], 0.05),
      ]),
    ).toEqual(["t3", "t4"]);
  });

  it("searches 20 threads by default", () => {
    const many = Array.from({ length: 30 }, (_, i) => hit(`t${i}`, [lookback], 0.9 - i / 100));
    expect(select(many, { search_limit: undefined, search_score_min: 0 })).toHaveLength(20);
    expect(searchBody([], {}).options.limit).toBe(80);
    for (const document of [candidateWorkflow, workflow]) {
      expect(document).toMatch(/search_limit: \{ type: integer, minimum: 1, default: 20 \}/);
    }
  });

  it("matches a thread without repo labels by path", () => {
    expect(select([hit("t1", ["path:/w/app"], 0.5), hit("t2", ["path:/other"], 0.5)])).toEqual(["t1"]);
  });

  const targets = (candidate_side: boolean, updated_thread_ids: string[]) =>
    runStep(candidateWorkflow, "pickTargets", "targets", {
      hit_threads: [{ thread_id: "t1" }, { thread_id: "t2" }, { thread_id: "t3" }],
      workflow: { input: { candidate_side, updated_thread_ids } },
    });

  it("judges every hit for an updated candidate and only updated hits otherwise", () => {
    expect(targets(true, [])).toEqual({ primary: ["t1", "t2", "t3"], others: [] });
    expect(targets(false, ["t2"])).toEqual({ primary: ["t2"], others: ["t1", "t3"] });
    expect(targets(false, ["t9"])).toEqual({ primary: [], others: ["t1", "t2", "t3"] });
  });

  it("reads the evidence range from the summary checkpoint", () => {
    const checkpoint = (metadata: unknown, updatedAt?: string) =>
      runStep(
        candidateWorkflow,
        "fetchThreadCheckpoint",
        "thread_checkpoint_at",
        {},
        { jsonBody: JSON.stringify([{ memory: { data: { metadata: JSON.stringify(metadata), updatedAt } } }]) },
      );
    expect(checkpoint({ resolution_high_water_message_at: "300", last_processed_message_at: "200" }, "900")).toBe(300);
    expect(checkpoint({ last_processed_message_at: 200 }, "900")).toBe(200);
    // Legacy summaries record no checkpoint; they covered the thread as it
    // was when the summary was last written.
    expect(checkpoint({ summary_version: "1.0" }, "900")).toBe(900);
    expect(checkpoint({})).toBeNull();
    // Legacy summaries are MEMORY_KIND_RAW without a content type, so the
    // lookup goes by the external ID alone.
    const request = JSON.parse(
      runStep(candidateWorkflow, "fetchThreadCheckpoint", "json_body", { checkpoint_thread_id: "t1" }) as string,
    );
    expect(request).toEqual({ external_id: "summary:t1" });
  });
});

describe("cross-thread verdicts", () => {
  const judged = (overrides: Record<string, unknown>) => ({
    transient_failure: false,
    evidence_ready: false,
    decision_payload: null,
    resolution_decision: "NO_CHANGE",
    skipped_candidates: [],
    failed_candidates: [],
    evidence_thread_ids: [],
    evidence_memory_ids: [],
    llm_outcome: null,
    ...overrides,
  });
  const classify = (output: Record<string, unknown>) =>
    (
      runStep(candidateWorkflow, "classifyVerdict", "verdicts", {
        verdicts: [],
        judged: judged(output),
        judge_thread_id: "t1",
        judge_index: 0,
        judge_targets: ["t1"],
      }) as Record<string, unknown>[]
    )[0];

  it("classifies the shared judge outcomes", () => {
    expect(
      classify({
        evidence_ready: true,
        decision_payload: { reject: {} },
        resolution_decision: "COMPLETE",
        evidence_thread_ids: ["t1"],
        evidence_memory_ids: ["m1"],
      }),
    ).toMatchObject({ thread_id: "t1", kind: "terminal", decision: "COMPLETE", ready: true });
    expect(
      classify({
        skipped_candidates: [{ reason: "confidence_below_threshold", llm: { decision: "CANCEL" } }],
        llm_outcome: { decision: "CANCEL", confidence: 0.3 },
      }),
    ).toMatchObject({ kind: "terminal", decision: "CANCEL", ready: false });
    expect(classify({ skipped_candidates: [{ reason: "no_resolution" }] })).toMatchObject({ kind: "keep" });
    expect(classify({ skipped_candidates: [{ reason: "cross_thread_raw_evidence_missing" }] })).toMatchObject({
      kind: "keep",
    });
    expect(classify({ skipped_candidates: [{ reason: "evidence_memory_ids_outside_input_window" }] })).toMatchObject({
      kind: "unverifiable",
      reason: "evidence_memory_ids_outside_input_window",
    });
    const llm_response = { length: 10, head: "{\"decision", tail: "{\"decision", input_messages: 3, input_chars: 900 };
    expect(
      classify({ failed_candidates: [{ reason: "invalid resolution LLM response", llm_response }] }),
    ).toMatchObject({ kind: "unverifiable", llm_response });
    expect(classify({ transient_failure: true, failed_candidates: [{ reason: "timeout" }] })).toMatchObject({
      kind: "transient",
    });
  });

  const ready = (thread_id: string, decision: string, memory: string) => ({
    thread_id,
    kind: "terminal",
    decision,
    ready: true,
    evidence_thread_ids: [thread_id],
    evidence_memory_ids: [memory],
    decision_audit: { rationale: "Cross-thread conversation evidence explicitly establishes completion." },
    decision_payload: { reject: { rationale: "fixed" } },
  });
  const aggregate = (verdicts: unknown[]) =>
    runStep(candidateWorkflow, "aggregateVerdicts", "aggregate", { verdicts }) as Record<string, unknown>;

  const again = (verdict: Record<string, unknown>) => ({ ...verdict, round: 2 });

  it("completes on one confident completion, with the evidence of every completing thread", () => {
    const result = aggregate([ready("t1", "COMPLETE", "m1"), { thread_id: "t2", kind: "keep" }, ready("t3", "COMPLETE", "m3")]);
    expect(result).toMatchObject({ kind: "apply", decision: "COMPLETE" });
    expect(result.audit_reason).toBe(
      "Cross-thread conversation evidence explicitly establishes completion. [evidence_thread_ids=t1,t3] [evidence_memory_ids=m1,m3]",
    );
    expect(result.decision_payload).toEqual({ reject: { rationale: "fixed" } });
  });

  it("completes even when other threads disagree, fail or could not be verified", () => {
    for (const other of [
      ready("t2", "CANCEL", "m2"),
      { thread_id: "t2", kind: "terminal", decision: "CANCEL", ready: false },
      { thread_id: "t2", kind: "transient" },
      { thread_id: "t2", kind: "unverifiable" },
    ]) {
      const result = aggregate([ready("t1", "COMPLETE", "m1"), other]);
      expect(result).toMatchObject({ kind: "apply", decision: "COMPLETE", evidence_thread_ids: ["t1"] });
    }
  });

  it("cancels only when every confident cancellation repeats", () => {
    const first = [ready("t1", "CANCEL", "m1")];
    expect(aggregate(first)).toMatchObject({ kind: "unverifiable", reason: "judgement_not_repeated" });
    expect(aggregate([...first, again({ thread_id: "t1", kind: "keep" })])).toMatchObject({
      kind: "unverifiable",
      reason: "judgement_not_repeated",
    });
    expect(aggregate([...first, again({ thread_id: "t1", kind: "transient" })]).kind).toBe("transient");
    expect(aggregate([...first, again(ready("t1", "CANCEL", "m9"))])).toMatchObject({
      kind: "apply",
      decision: "CANCEL",
    });
  });

  it("does not cancel against a contradicting, failed or unverifiable judgement", () => {
    const cancel = [ready("t1", "CANCEL", "m1"), again(ready("t1", "CANCEL", "m1"))];
    expect(
      aggregate([...cancel, { thread_id: "t2", kind: "terminal", decision: "COMPLETE", ready: false }]).kind,
    ).toBe("contradicted");
    expect(aggregate([...cancel, { thread_id: "t2", kind: "transient" }]).kind).toBe("transient");
    expect(aggregate([...cancel, { thread_id: "t2", kind: "unverifiable" }]).kind).toBe("unverifiable");
  });

  it("keeps a candidate without a confident terminal decision", () => {
    expect(aggregate([{ thread_id: "t1", kind: "keep" }]).kind).toBe("keep");
    expect(aggregate([{ thread_id: "t1", kind: "terminal", decision: "COMPLETE", ready: false }]).kind).toBe("keep");
    expect(aggregate([]).kind).toBe("keep");
  });

  const judges = (verdicts: unknown[], judge_thread_id: string, judge_index: number) =>
    runStep(candidateWorkflow, "judgeThread", "if", {
      verdicts,
      judge_thread_id,
      judge_index,
      judge_targets: ["t1", "t2"],
      targets: { primary: ["t1"], others: ["t2"] },
    });

  it("judges a thread a second time only for a cancellation the first round would write", () => {
    expect(judges([ready("t1", "CANCEL", "m1"), { thread_id: "t2", kind: "keep" }], "t1", 2)).toBe(true);
    expect(judges([ready("t1", "COMPLETE", "m1"), { thread_id: "t2", kind: "keep" }], "t1", 2)).toBe(false);
    expect(judges([{ thread_id: "t1", kind: "keep" }], "t1", 2)).toBe(false);
    expect(judges([ready("t1", "CANCEL", "m1"), { thread_id: "t2", kind: "unverifiable" }], "t1", 2)).toBe(false);
    expect(
      runStep(candidateWorkflow, "classifyVerdict", "verdicts", {
        verdicts: [],
        judge_thread_id: "t1",
        judge_index: 2,
        judge_targets: ["t1", "t2"],
        judged: { skipped_candidates: [{ reason: "no_resolution" }] },
      }),
    ).toMatchObject([{ thread_id: "t1", round: 2 }]);
    expect(stepExpression(candidateWorkflow, "judgeThreads", "in")).toContain("$judge_targets + $judge_targets");
  });

  it("stops judging once a thread confidently completes the candidate", () => {
    const completed = [ready("t1", "COMPLETE", "m1")];
    expect(judges(completed, "t2", 1)).toBe(false);
    expect(
      runStep(candidateWorkflow, "judgeThread", "if", {
        verdicts: completed,
        judge_thread_id: "t2",
        judge_index: 1,
        judge_targets: ["t1", "t2"],
        targets: { primary: ["t1", "t2"], others: [] },
      }),
    ).toBe(false);
  });

  it("confirms with the remaining hits only after a confident cancellation", () => {
    expect(judges([ready("t1", "CANCEL", "m1")], "t2", 1)).toBe(true);
    expect(judges([{ thread_id: "t1", kind: "keep" }], "t2", 1)).toBe(false);
    expect(judges([{ thread_id: "t1", kind: "terminal", decision: "CANCEL", ready: false }], "t2", 1)).toBe(false);
    // Updated threads are always judged.
    expect(judges([], "t1", 0)).toBe(true);
    const order = stepExpression(candidateWorkflow, "listJudgeTargets", "judge_targets");
    expect(order).toContain("$targets.primary + $targets.others");
  });

  it("tells the judge that a thread about other work is no evidence either way", () => {
    const input = blockBetween(candidateWorkflow, "- invokeJudge:", "export:");
    expect(input).toMatch(/evidence_scope: "[^"]*other work[^"]*NO_CHANGE[^"]*"/);
  });

  it("writes with its own actor and a key stable across retries of the same decision", () => {
    const input = JSON.parse(
      runStep(candidateWorkflow, "invokeApply", "input", {
        candidate_id: "c1",
        candidate_revision: 3,
        aggregate: { decision: "COMPLETE", decision_payload: { reject: {} }, audit_reason: "reason" },
        workflow: { input: { issue_tracker_grpc_host: "h", issue_tracker_grpc_port: 1, manager_contract: {} } },
      }) as string,
    );
    expect(input.actor_id).toBe("lookback-cross-thread-issue-resolution");
    expect(input.idempotency_key).toBe("cross-thread-issue-resolution:c1:3:COMPLETE");
    expect(input.audit_reason).toBe("reason");
  });
});

describe("cross-thread dry-run evaluation", () => {
  const evaluation = (dry_run: boolean, extra: Record<string, unknown> = {}) =>
    evaluate(outputExpression(candidateWorkflow, "evaluation"), {
      workflow: { input: { dry_run } },
      candidate_id: "c1",
      candidate: { title: "Fix parser" },
      hit_threads: [{ thread_id: "t1", score: 0.5, labels: ["x"], last_message_at: 1 }],
      verdicts: [
        {
          thread_id: "t1",
          round: 1,
          kind: "keep",
          confidence: 0.95,
          reasons: ["no_resolution"],
          decision_payload: { big: 1 },
        },
      ],
      aggregate: { kind: "keep" },
      unmatched: false,
      unmatched_reason: null,
      search_mode: "text",
      ...extra,
    }) as Record<string, unknown> | null;

  it("reports each candidate's hits and per-thread judgements in a dry run only", () => {
    expect(evaluation(false)).toBeNull();
    expect(evaluation(true)).toEqual({
      candidate_id: "c1",
      title: "Fix parser",
      outcome: "keep",
      search_mode: "text",
      hits: [{ thread_id: "t1", score: 0.5 }],
      verdicts: [{ thread_id: "t1", kind: "keep", confidence: 0.95, reasons: ["no_resolution"], round: 1 }],
    });
    expect(
      (evaluation(true, { unmatched: true, unmatched_reason: "no_summary_checkpoint", aggregate: null }) as {
        outcome: string;
      }).outcome,
    ).toBe("unmatched:no_summary_checkpoint");
  });

  it("reports the shape of an invalid LLM response", () => {
    const llm_response = { length: 0, head: "", tail: "", input_messages: 3, input_chars: 900 };
    const verdicts = [{ thread_id: "t1", round: 1, kind: "unverifiable", llm_response }];
    expect((evaluation(true, { verdicts }) as { verdicts: unknown[] }).verdicts).toEqual(verdicts);
  });

  it("does not keep a candidate no thread could judge", () => {
    const skips = (checkpoints: Record<string, number>) =>
      runStep(candidateWorkflow, "skipWithoutCheckpointedTarget", "if", {
        candidate_outcome_recorded: false,
        targets: { primary: ["t1"], others: ["t2"] },
        checkpoints,
      });
    expect(skips({})).toBe(true);
    // Only the other hits have one: they confirm, they cannot start a judgement.
    expect(skips({ t2: 5 })).toBe(true);
    expect(skips({ t1: 5 })).toBe(false);
    for (const [step, reason] of [
      ["skipWithoutTarget", "no_updated_hit"],
      ["skipWithoutLaterTarget", "no_thread_after_origin"],
      ["skipWithoutCheckpointedTarget", "no_summary_checkpoint"],
    ]) {
      expect(runStep(candidateWorkflow, step, "unmatched_reason", {})).toBe(reason);
    }
  });

  it("keeps the judge's confidence and reasons on every verdict", () => {
    const [verdict] = runStep(candidateWorkflow, "classifyVerdict", "verdicts", {
      verdicts: [],
      judge_thread_id: "t1",
      judge_index: 0,
      judge_targets: ["t1"],
      judged: {
        skipped_candidates: [{ reason: "no_resolution" }],
        failed_candidates: [],
        llm_outcome: { decision: "NO_CHANGE", confidence: 0.95 },
        resolution_rationale: "the thread works on something else",
      },
    }) as Record<string, unknown>[];
    expect(verdict).toMatchObject({
      kind: "keep",
      confidence: 0.95,
      reasons: ["no_resolution"],
      rationale: "the thread works on something else",
    });
  });

  it("collects the evaluations of every candidate in the run output", () => {
    expect(
      runStep(workflow, "mergeCandidateResult", "evaluations", {
        evaluations: [{ candidate_id: "a" }],
        candidate_result: { evaluation: { candidate_id: "b" } },
      }),
    ).toEqual([{ candidate_id: "a" }, { candidate_id: "b" }]);
    expect(
      runStep(workflow, "mergeCandidateResult", "evaluations", {
        evaluations: [],
        candidate_result: { evaluation: null },
      }),
    ).toEqual([]);
    expect(outputExpression(workflow, "evaluations")).toBe("$evaluations");
  });
});

describe("cross-thread run output", () => {
  it("counts every candidate outcome and is complete only without transient failures", () => {
    const bindings = {
      applied_candidates: [{}],
      dry_run_candidates: [{}],
      kept_candidates: [{}, {}],
      contradicted_candidates: [{}],
      protected_candidates: [],
      skipped_candidates: [{}],
      conflicted_candidates: [],
      failed_candidates: [],
    };
    expect(evaluate(outputExpression(workflow, "processed_count"), bindings)).toBe(6);
    expect(evaluate(outputExpression(workflow, "completed"), bindings)).toBe(true);
    expect(evaluate(outputExpression(workflow, "completed"), { ...bindings, failed_candidates: [{}] })).toBe(false);
  });

  it("merges each candidate worker's lists", () => {
    const merged = runStep(workflow, "mergeCandidateResult", "contradicted_candidates", {
      contradicted_candidates: [{ candidate_id: "a" }],
      candidate_result: { contradicted_candidates: [{ candidate_id: "b" }] },
    });
    expect(merged).toEqual([{ candidate_id: "a" }, { candidate_id: "b" }]);
  });

  it("registers durable, single-slot workers and a stored watermark", () => {
    const registration = (name: string) => blockBetween(extractWorkers, `name: ${name}\n`, "$file:");
    const auto = registration("lookback-cross-thread-issue-resolution");
    expect(auto).toContain("channel: issue_resolution_cross");
    expect(auto).toContain("queue_type: WITH_BACKUP");
    expect(registration("lookback-cross-thread-issue-resolution-candidate")).toContain(
      "channel: issue_resolution_cross_candidate",
    );
    const watermark = blockBetween(extractWorkers, "name: lookback-cross-thread-issue-resolution-watermark\n", "settings");
    expect(watermark).toContain("store_success: true");
    expect(watermark).toContain("runner: COMMAND");
  });
});
