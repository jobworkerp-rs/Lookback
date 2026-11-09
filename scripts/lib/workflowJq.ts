import { spawnSync } from "node:child_process";
import { expect } from "vitest";

// Returns the text from `startMarker` up to (not including) the next
// `endMarker`, or to the end when `endMarker` is omitted. Asserts both markers
// exist so a renamed step fails the test instead of silently slicing from -1.
export function blockBetween(source: string, startMarker: string, endMarker?: string): string {
  const start = source.indexOf(startMarker);
  expect(start, `missing ${startMarker}`).toBeGreaterThan(-1);
  if (endMarker === undefined) return source.slice(start);
  const end = source.indexOf(endMarker, start + startMarker.length);
  expect(end, `missing ${endMarker} after ${startMarker}`).toBeGreaterThan(start);
  return source.slice(start, end);
}

// Extracts the jq expression stored under `key` inside the task `- step:`.
// Handles both quoted inline values and `>-` folded scalars.
export function stepExpression(workflow: string, step: string, key: string): string {
  const start = workflow.indexOf(`- ${step}:`);
  expect(start, `step ${step} not found`).toBeGreaterThan(-1);
  return expressionAfter(workflow.slice(start), key, `${step}.${key}`);
}

// Extracts the jq expression stored under `key` in the workflow's top-level
// `output:` block.
export function outputExpression(workflow: string, key: string): string {
  const start = workflow.lastIndexOf("\noutput:");
  expect(start, "workflow output block is present").toBeGreaterThanOrEqual(0);
  return expressionAfter(workflow.slice(start), key, `output.${key}`);
}

function expressionAfter(block: string, key: string, label: string): string {
  const lines = block.split("\n");
  // Only expression-valued keys count: an LLM prompt inside the step may
  // contain the same key as literal Liquid text (e.g. `summary_json: {{ … }}`).
  const keyPattern = new RegExp(`^(\\s+)${key}:\\s*(>-|["']?\\$\\{.*)$`);
  const index = lines.findIndex((line, i) => i > 0 && keyPattern.test(line));
  expect(index, `${label} not found`).toBeGreaterThan(0);
  const [, indent, inline] = keyPattern.exec(lines[index]) ?? [];
  let raw: string;
  if (inline === ">-") {
    const body: string[] = [];
    for (const line of lines.slice(index + 1)) {
      if (line.trim() !== "" && line.length - line.trimStart().length <= indent.length) break;
      body.push(line);
    }
    raw = body.join("\n").trim();
  } else {
    raw = inline
      .trim()
      .replace(/^"(.*)"$/s, "$1")
      .replace(/^'(.*)'$/s, "$1")
      .replace(/\\"/g, '"');
  }
  expect(raw.startsWith("${"), `${label} is not a jq expression`).toBe(true);
  return raw.slice(2).replace(/}\s*$/, "").trim();
}

// Evaluates a workflow jq expression with `$name` bindings, as the engine does.
// `input` becomes `.` (e.g. a task's raw output); omit it for set expressions.
export function evaluate(
  expression: string,
  variables: Record<string, unknown>,
  input?: unknown,
): unknown {
  const args = input === undefined ? ["-n", "-c"] : ["-c"];
  for (const [name, value] of Object.entries(variables)) {
    args.push("--argjson", name, JSON.stringify(value));
  }
  const result = spawnSync("jq", [...args, expression], {
    encoding: "utf8",
    input: input === undefined ? undefined : JSON.stringify(input),
  });
  expect(result.error).toBeUndefined();
  expect(result.status, result.stderr).toBe(0);
  return JSON.parse(result.stdout.trim());
}

// Evaluates the jq expression a workflow task stores under `key`.
export const runStep = (
  workflow: string,
  step: string,
  key: string,
  bindings: Record<string, unknown>,
  input?: unknown,
): unknown => evaluate(stepExpression(workflow, step, key), bindings, input);

// Extracts the `if:` predicate of a `raise` guard task (`- step:` ... `raise:`).
// The predicate is the first `${ ... }` after the step name and ends at the
// `raise:` line, so folded and quoted scalars are both handled.
export function stepCondition(workflow: string, step: string): string {
  const start = workflow.indexOf(`- ${step}:`);
  expect(start, `step ${step} not found`).toBeGreaterThan(-1);
  const from = workflow.indexOf("${", start);
  const end = /\n\s+raise:/.exec(workflow.slice(from));
  const to = end == null ? -1 : from + end.index;
  expect(to, `raise: not found after ${step}`).toBeGreaterThan(from);
  return workflow
    .slice(from + 2, to)
    .trim()
    .replace(/}\s*'?\s*$/, "");
}
