import type {
  CodingAgentApprovalAssessmentContract,
  CodingAgentValidationDraftRequest,
} from "@/types/api";

export function parseValidationCommands(
  text: string,
  workingDirectory: string,
  timeoutSeconds: number,
  required: boolean,
): CodingAgentValidationDraftRequest[] | null {
  if (!text.trim()) return [];
  if (
    text.length > 128 * 64 * 1024 ||
    !Number.isInteger(timeoutSeconds) ||
    timeoutSeconds <= 0 ||
    timeoutSeconds > 10800 ||
    (workingDirectory !== "/workspace" && !workingDirectory.startsWith("/workspace/")) ||
    Array.from(workingDirectory).some(
      (character) =>
        character.charCodeAt(0) < 32 ||
        character.charCodeAt(0) === 127 ||
        character === String.fromCharCode(92),
    ) ||
    workingDirectory
      .split("/")
      .slice(1)
      .some((part) => !part || part === "." || part === "..")
  )
    return null;
  const lines = text.split("\n").filter((line) => line.trim());
  if (lines.length > 128) return null;
  const commands: CodingAgentValidationDraftRequest[] = [];
  const encoder = new TextEncoder();
  for (const line of lines) {
    let value: unknown;
    try {
      value = JSON.parse(line);
    } catch {
      return null;
    }
    if (!Array.isArray(value) || value.length === 0 || value.length > 128) return null;
    const argv: string[] = [];
    let bytes = 0;
    for (const argument of value) {
      if (typeof argument !== "string" || argument.includes("\u0000")) return null;
      const size = encoder.encode(argument).length;
      if (size > 16 * 1024) return null;
      bytes += size;
      argv.push(argument);
    }
    if (!argv[0] || bytes > 64 * 1024) return null;
    commands.push({
      required,
      command: { argv, workingDirectory, timeoutMs: timeoutSeconds * 1000 },
    });
  }
  return commands;
}

export function validationDraftsMatchPreview(
  contract: CodingAgentApprovalAssessmentContract,
  requested: CodingAgentValidationDraftRequest[] | undefined,
): boolean {
  if (requested === undefined) return true;
  if (requested.length !== contract.validation_plan.length) return false;
  return requested.every((draft, index) => {
    const definition = contract.validation_plan[index];
    if (!definition || draft.required !== definition.required) return false;
    if (draft.command == null) return definition.command === null;
    const command = definition.command;
    return (
      command !== null &&
      command.working_directory === draft.command.workingDirectory &&
      command.timeout_ms === draft.command.timeoutMs &&
      JSON.stringify(command.argv) === JSON.stringify(draft.command.argv)
    );
  });
}
