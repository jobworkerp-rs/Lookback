import { expect, it } from "vitest";
import { parseValidationCommands, validationDraftsMatchPreview } from "./validationCommands";

it("parses explicit argv without shell expansion and maps timeout to milliseconds", () => {
  const commands = parseValidationCommands(
    '["python","-c","print(1)\\nprint(2)",""]\n["cargo","test"]',
    "/workspace/crate",
    43,
    true,
  );
  expect(commands).toEqual([
    {
      required: true,
      command: {
        argv: ["python", "-c", "print(1)\nprint(2)", ""],
        workingDirectory: "/workspace/crate",
        timeoutMs: 43000,
      },
    },
    {
      required: true,
      command: { argv: ["cargo", "test"], workingDirectory: "/workspace/crate", timeoutMs: 43000 },
    },
  ]);
  expect(parseValidationCommands("", "/workspace", 43, false)).toEqual([]);
});
it("rejects malformed command data, path traversal, NUL and invalid deadlines", () => {
  for (const text of ["cargo test", "[]", '[""]', "[42]", '["x","\\u0000"]']) {
    expect(parseValidationCommands(text, "/workspace", 43, true)).toBeNull();
  }
  for (const cwd of ["/host", "/workspace/../host", "/workspace//crate"]) {
    expect(parseValidationCommands('["cargo","test"]', cwd, 43, true)).toBeNull();
  }
  for (const timeout of [0, 10801, NaN, 1.1]) {
    expect(parseValidationCommands('["cargo","test"]', "/workspace", timeout, true)).toBeNull();
  }
});

it("rejects a returned plan that omits or changes any explicitly requested validation field", () => {
  const requested = parseValidationCommands('["cargo","test"]', "/workspace", 43, true);
  if (!requested) throw new Error("fixture commands are invalid");
  const definition = {
    validation_id: "V-1",
    required: true,
    command: {
      argv: ["cargo", "test"],
      working_directory: "/workspace",
      timeout_ms: 43000,
      permissions: ["read_workspace" as const],
      profile_digest: "a".repeat(64),
    },
  };
  const contract = {
    schema_version: 1,
    purpose: "implementation" as const,
    criteria: [],
    validation_plan: [definition],
    provider: null,
    prompt_policy_version: null,
    input_scope: { allowed_fields: [], allowed_sources: [], max_input_bytes: 1 },
    judge_input_scope_digest: "b".repeat(64),
  };
  expect(validationDraftsMatchPreview(contract, requested)).toBe(true);
  for (const changed of [
    [],
    [{ ...definition, required: false }],
    [{ ...definition, command: { ...definition.command, argv: ["cargo", "check"] } }],
    [{ ...definition, command: { ...definition.command, working_directory: "/workspace/crate" } }],
    [{ ...definition, command: { ...definition.command, timeout_ms: 1000 } }],
  ]) {
    expect(validationDraftsMatchPreview({ ...contract, validation_plan: changed }, requested)).toBe(
      false,
    );
  }
});
