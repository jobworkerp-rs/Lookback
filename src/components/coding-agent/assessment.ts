import type {
  CodingAgentApprovalAssessmentContract,
  CodingAgentApprovedAcceptanceCriterion,
  CodingAgentApprovedJudgeInputField,
  CodingAgentApprovedValidationDefinition,
  CodingAgentAssessmentInputScope,
  CodingAgentEvidenceSourceKind,
  CodingAgentProviderSelection,
  CodingAgentValidationPermission,
} from "@/types/api";

const MAX_ASSESSMENT_ITEMS = 128;
const MAX_IDENTIFIER_BYTES = 256;
const MAX_TEXT_BYTES = 16 * 1024;
const MAX_REPORT_BYTES = 1024 * 1024;
const MAX_EVIDENCE_REFERENCES = 512;
const MAX_INPUT_BYTES = 128 * 1024;
const MAX_JSON_NESTING = 64;
const MAX_JSON_VALUE_TOKENS = 100_000;
const MAX_JSON_OBJECT_KEYS = 16_384;
const SHA256_PATTERN = /^[a-f0-9]{64}$/;

const approvedInputFields: readonly CodingAgentApprovedJudgeInputField[] = [
  "issue_title",
  "issue_body",
  "repository_identity",
  "repository_summary",
  "thread_group_context",
  "bounded_file",
  "bounded_diff",
  "report",
  "validation_result",
];

const evidenceSourceKinds: readonly CodingAgentEvidenceSourceKind[] = [
  "issue_context",
  "repository_context",
  "thread_group_context",
  "file",
  "diff",
  "report",
  "validation_result",
];

export interface CodingAgentAgentReport {
  schema_version: 1;
  run_id: string;
  attempt_id: string;
  task_kind: "investigation" | "implementation";
  status: "completed" | "partial" | "blocked";
  summary: string;
  acceptance: Array<{
    criterion_id: string;
    assessment: "met" | "unmet" | "unknown";
    evidence: string[];
  }>;
  validation: Array<{
    validation_id: string;
    claimed_result: "passed" | "failed" | "not_run" | "unknown";
    evidence: string[];
    reason: string;
  }>;
  investigation: {
    scope: string[];
    findings: Array<{ summary: string; criterion_ids: string[]; evidence: string[] }>;
    limitations: string[];
    response_questions: string[];
  } | null;
  implementation: {
    change_summary: string;
    changed_files: string[];
    implementation_notes: string[];
  } | null;
  unresolved: Array<{
    description: string;
    blocks_completion: boolean;
    criterion_ids: string[];
  }>;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasExactKeys(value: Record<string, unknown>, keys: readonly string[]): boolean {
  const actual = Object.keys(value);
  return actual.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

function byteLength(value: string): number {
  return new TextEncoder().encode(value).byteLength;
}

function isBoundedText(value: unknown, allowBlank = true): value is string {
  return (
    typeof value === "string" &&
    byteLength(value) <= MAX_TEXT_BYTES &&
    (allowBlank || value.trim().length > 0)
  );
}

function isIdentifier(value: unknown): value is string {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    byteLength(value) <= MAX_IDENTIFIER_BYTES &&
    /^[A-Za-z0-9_.:/@+-]+$/.test(value)
  );
}

function isSha256(value: unknown): value is string {
  return typeof value === "string" && SHA256_PATTERN.test(value);
}

function isUnique(values: readonly string[]): boolean {
  return new Set(values).size === values.length;
}

function isCriterion(
  value: unknown,
  index: number,
): value is CodingAgentApprovedAcceptanceCriterion {
  if (
    !isRecord(value) ||
    !hasExactKeys(value, ["criterion_id", "statement", "required", "method"])
  ) {
    return false;
  }
  return (
    value.criterion_id === `AC-${index + 1}` &&
    isBoundedText(value.statement, false) &&
    typeof value.required === "boolean" &&
    (value.method === "deterministic" ||
      value.method === "semantic_judge" ||
      value.method === "unknown")
  );
}

function isValidationCommand(value: unknown): boolean {
  if (
    !isRecord(value) ||
    !hasExactKeys(value, [
      "argv",
      "working_directory",
      "permissions",
      "profile_digest",
      "timeout_ms",
    ])
  ) {
    return false;
  }
  const argv = value.argv;
  const permissions = value.permissions;
  if (
    !Array.isArray(argv) ||
    argv.length === 0 ||
    argv[0] === "" ||
    argv.length > MAX_ASSESSMENT_ITEMS ||
    !argv.every((argument) => isBoundedText(argument) && !argument.includes("\u0000")) ||
    argv.reduce((total, argument) => total + byteLength(argument), 0) > 64 * 1024 ||
    !isBoundedText(value.working_directory, false) ||
    (value.working_directory !== "/workspace" &&
      !value.working_directory.startsWith("/workspace/")) ||
    value.working_directory.includes("\\") ||
    value.working_directory.split("/").some((part) => part === "." || part === "..") ||
    !Array.isArray(permissions) ||
    permissions.length === 0 ||
    permissions.length > 2 ||
    !permissions.every(
      (permission): permission is CodingAgentValidationPermission =>
        permission === "read_workspace" || permission === "write_build_output",
    ) ||
    !isUnique(permissions) ||
    !permissions.includes("read_workspace") ||
    !isSha256(value.profile_digest) ||
    !Number.isSafeInteger(value.timeout_ms) ||
    (value.timeout_ms as number) <= 0 ||
    (value.timeout_ms as number) > 10800 * 1000
  ) {
    return false;
  }
  return true;
}

function isValidationDefinition(
  value: unknown,
  index: number,
): value is CodingAgentApprovedValidationDefinition {
  if (
    !isRecord(value) ||
    !hasExactKeys(value, ["validation_id", "required", "command"]) ||
    value.validation_id !== `V-${index + 1}` ||
    typeof value.required !== "boolean"
  ) {
    return false;
  }
  return value.command === null || isValidationCommand(value.command);
}

function isInputScope(value: unknown): value is CodingAgentAssessmentInputScope {
  if (
    !isRecord(value) ||
    !hasExactKeys(value, ["allowed_fields", "allowed_sources", "max_input_bytes"])
  ) {
    return false;
  }
  const fields = value.allowed_fields;
  const sources = value.allowed_sources;
  if (
    !Array.isArray(fields) ||
    fields.length > 32 ||
    !fields.every((field) =>
      approvedInputFields.includes(field as CodingAgentApprovedJudgeInputField),
    ) ||
    !isUnique(fields as string[]) ||
    !Array.isArray(sources) ||
    sources.length > MAX_ASSESSMENT_ITEMS ||
    !Number.isSafeInteger(value.max_input_bytes) ||
    (value.max_input_bytes as number) <= 0 ||
    (value.max_input_bytes as number) > MAX_INPUT_BYTES
  ) {
    return false;
  }

  const identities: string[] = [];
  for (const source of sources) {
    if (
      !isRecord(source) ||
      !hasExactKeys(source, ["kind", "identity", "max_bytes"]) ||
      !evidenceSourceKinds.includes(source.kind as CodingAgentEvidenceSourceKind) ||
      !isIdentifier(source.identity) ||
      !Number.isSafeInteger(source.max_bytes) ||
      (source.max_bytes as number) <= 0
    ) {
      return false;
    }
    const maxAllowed = source.kind === "report" ? MAX_REPORT_BYTES : 32 * 1024;
    if ((source.max_bytes as number) > maxAllowed) return false;
    identities.push(`${source.kind}\0${source.identity}`);
  }
  return isUnique(identities);
}

function isProviderSelection(value: unknown): value is CodingAgentProviderSelection {
  return (
    isRecord(value) &&
    hasExactKeys(value, ["provider_id", "model_id", "model_version", "config_version"]) &&
    isIdentifier(value.provider_id) &&
    isIdentifier(value.model_id) &&
    isIdentifier(value.model_version) &&
    isIdentifier(value.config_version)
  );
}

/** Validate the frozen, snake_case contract without deriving data from display-only Issue text. */
export function isCodingAgentApprovalAssessmentContract(
  value: unknown,
  contractDigest: unknown,
  providerSupported: unknown,
): value is CodingAgentApprovalAssessmentContract {
  if (
    !isRecord(value) ||
    !hasExactKeys(value, [
      "schema_version",
      "purpose",
      "criteria",
      "validation_plan",
      "provider",
      "prompt_policy_version",
      "input_scope",
      "judge_input_scope_digest",
    ]) ||
    value.schema_version !== 1 ||
    (value.purpose !== "investigation" && value.purpose !== "implementation") ||
    !Array.isArray(value.criteria) ||
    value.criteria.length > MAX_ASSESSMENT_ITEMS ||
    !value.criteria.every(isCriterion) ||
    !Array.isArray(value.validation_plan) ||
    value.validation_plan.length > MAX_ASSESSMENT_ITEMS ||
    !value.validation_plan.every(isValidationDefinition) ||
    (value.provider !== null && !isProviderSelection(value.provider)) ||
    (value.prompt_policy_version !== null && !isIdentifier(value.prompt_policy_version)) ||
    !isInputScope(value.input_scope) ||
    !isSha256(value.judge_input_scope_digest) ||
    !isSha256(contractDigest) ||
    typeof providerSupported !== "boolean" ||
    providerSupported !== (value.provider !== null)
  ) {
    return false;
  }
  return true;
}

/** Canonical criteria are a vector; the legacy joined display string is never split or used here. */
export function approvalContractMatchesCanonicalCriteria(
  contract: CodingAgentApprovalAssessmentContract,
  canonicalCriteria: unknown,
): boolean {
  if (
    !Array.isArray(canonicalCriteria) ||
    canonicalCriteria.length !== contract.criteria.length ||
    !canonicalCriteria.every((criterion) => typeof criterion === "string")
  ) {
    return false;
  }
  return contract.criteria.every(
    (criterion, index) => canonicalCriteria[index] === criterion.statement,
  );
}

function isStringArray(value: unknown, identifiers = false): value is string[] {
  return (
    Array.isArray(value) &&
    value.length <= MAX_ASSESSMENT_ITEMS &&
    value.every((item) => (identifiers ? isIdentifier(item) : isBoundedText(item)))
  );
}

function addEvidenceCount(current: number, values: string[]): number | null {
  const total = current + values.length;
  return total <= MAX_EVIDENCE_REFERENCES ? total : null;
}

function isAgentReport(value: unknown): value is CodingAgentAgentReport {
  if (
    !isRecord(value) ||
    !hasExactKeys(value, [
      "schema_version",
      "run_id",
      "attempt_id",
      "task_kind",
      "status",
      "summary",
      "acceptance",
      "validation",
      "investigation",
      "implementation",
      "unresolved",
    ]) ||
    value.schema_version !== 1 ||
    !isIdentifier(value.run_id) ||
    !isIdentifier(value.attempt_id) ||
    (value.task_kind !== "investigation" && value.task_kind !== "implementation") ||
    (value.status !== "completed" && value.status !== "partial" && value.status !== "blocked") ||
    !isBoundedText(value.summary) ||
    !Array.isArray(value.acceptance) ||
    value.acceptance.length > MAX_ASSESSMENT_ITEMS ||
    !Array.isArray(value.validation) ||
    value.validation.length > MAX_ASSESSMENT_ITEMS ||
    !Array.isArray(value.unresolved) ||
    value.unresolved.length > MAX_ASSESSMENT_ITEMS
  ) {
    return false;
  }

  if (
    (value.task_kind === "implementation" &&
      (value.investigation !== null || value.implementation === null)) ||
    (value.task_kind === "investigation" &&
      (value.implementation !== null || value.investigation === null))
  ) {
    return false;
  }

  let evidenceCount = 0;
  const criterionIds: string[] = [];
  for (const item of value.acceptance) {
    if (
      !isRecord(item) ||
      !hasExactKeys(item, ["criterion_id", "assessment", "evidence"]) ||
      !isIdentifier(item.criterion_id) ||
      (item.assessment !== "met" && item.assessment !== "unmet" && item.assessment !== "unknown") ||
      !isStringArray(item.evidence)
    ) {
      return false;
    }
    criterionIds.push(item.criterion_id);
    const next = addEvidenceCount(evidenceCount, item.evidence);
    if (next === null) return false;
    evidenceCount = next;
  }
  if (!isUnique(criterionIds)) return false;

  const validationIds: string[] = [];
  for (const item of value.validation) {
    if (
      !isRecord(item) ||
      !hasExactKeys(item, ["validation_id", "claimed_result", "evidence", "reason"]) ||
      !isIdentifier(item.validation_id) ||
      (item.claimed_result !== "passed" &&
        item.claimed_result !== "failed" &&
        item.claimed_result !== "not_run" &&
        item.claimed_result !== "unknown") ||
      !isStringArray(item.evidence) ||
      !isBoundedText(item.reason)
    ) {
      return false;
    }
    validationIds.push(item.validation_id);
    const next = addEvidenceCount(evidenceCount, item.evidence);
    if (next === null) return false;
    evidenceCount = next;
  }
  if (!isUnique(validationIds)) return false;

  if (value.investigation !== null) {
    const investigation = value.investigation;
    if (
      !isRecord(investigation) ||
      !hasExactKeys(investigation, ["scope", "findings", "limitations", "response_questions"]) ||
      !isStringArray(investigation.scope) ||
      !Array.isArray(investigation.findings) ||
      investigation.findings.length > MAX_ASSESSMENT_ITEMS ||
      !isStringArray(investigation.limitations) ||
      !isStringArray(investigation.response_questions)
    ) {
      return false;
    }
    for (const finding of investigation.findings) {
      if (
        !isRecord(finding) ||
        !hasExactKeys(finding, ["summary", "criterion_ids", "evidence"]) ||
        !isBoundedText(finding.summary) ||
        !isStringArray(finding.criterion_ids, true) ||
        !isStringArray(finding.evidence)
      ) {
        return false;
      }
      const next = addEvidenceCount(evidenceCount, finding.evidence);
      if (next === null) return false;
      evidenceCount = next;
    }
  }

  if (value.implementation !== null) {
    const implementation = value.implementation;
    if (
      !isRecord(implementation) ||
      !hasExactKeys(implementation, ["change_summary", "changed_files", "implementation_notes"]) ||
      !isBoundedText(implementation.change_summary) ||
      !isStringArray(implementation.changed_files) ||
      !isStringArray(implementation.implementation_notes)
    ) {
      return false;
    }
  }

  for (const item of value.unresolved) {
    if (
      !isRecord(item) ||
      !hasExactKeys(item, ["description", "blocks_completion", "criterion_ids"]) ||
      !isBoundedText(item.description) ||
      typeof item.blocks_completion !== "boolean" ||
      !isStringArray(item.criterion_ids, true)
    ) {
      return false;
    }
  }
  return true;
}

/** Scan JSON syntax before JSON.parse, rejecting duplicate decoded keys without building an object tree. */
function hasUniqueJsonObjectKeys(source: string): boolean {
  let offset = 0;
  let valueTokenCount = 0;
  let objectKeyCount = 0;

  const skipWhitespace = () => {
    while (offset < source.length) {
      const code = source.charCodeAt(offset);
      if (code !== 0x09 && code !== 0x0a && code !== 0x0d && code !== 0x20) break;
      offset += 1;
    }
  };

  const parseStringToken = (): string | null => {
    if (source[offset] !== '"') return null;
    const start = offset;
    offset += 1;
    while (offset < source.length) {
      const code = source.charCodeAt(offset);
      if (code === 34) {
        offset += 1;
        try {
          return JSON.parse(source.slice(start, offset)) as string;
        } catch {
          return null;
        }
      }
      if (code < 32) return null;
      if (code === 92) {
        offset += 2;
      } else {
        offset += 1;
      }
    }
    return null;
  };

  const parseNumberToken = (): boolean => {
    if (source[offset] === "-") offset += 1;
    const firstDigit = source[offset];
    if (firstDigit === "0") {
      offset += 1;
      if (/[0-9]/.test(source[offset] ?? "")) return false;
    } else if (firstDigit !== undefined && firstDigit >= "1" && firstDigit <= "9") {
      offset += 1;
      while (/[0-9]/.test(source[offset] ?? "")) offset += 1;
    } else {
      return false;
    }

    if (source[offset] === ".") {
      offset += 1;
      if (!/[0-9]/.test(source[offset] ?? "")) return false;
      while (/[0-9]/.test(source[offset] ?? "")) offset += 1;
    }
    if (source[offset] === "e" || source[offset] === "E") {
      offset += 1;
      if (source[offset] === "+" || source[offset] === "-") offset += 1;
      if (!/[0-9]/.test(source[offset] ?? "")) return false;
      while (/[0-9]/.test(source[offset] ?? "")) offset += 1;
    }
    return true;
  };

  const parseValue = (containerDepth: number): boolean => {
    valueTokenCount += 1;
    if (valueTokenCount > MAX_JSON_VALUE_TOKENS) return false;
    skipWhitespace();

    const token = source[offset];
    if (token === '"') return parseStringToken() !== null;
    if (token === "{" || token === "[") {
      if (containerDepth >= MAX_JSON_NESTING) return false;
      const isObject = token === "{";
      const closing = isObject ? "}" : "]";
      offset += 1;
      skipWhitespace();
      if (source[offset] === closing) {
        offset += 1;
        return true;
      }

      const keys = isObject ? new Set<string>() : null;
      while (offset < source.length) {
        if (isObject) {
          const key = parseStringToken();
          if (key === null || !keys) return false;
          objectKeyCount += 1;
          if (objectKeyCount > MAX_JSON_OBJECT_KEYS || keys.has(key)) return false;
          keys.add(key);
          skipWhitespace();
          if (source[offset] !== ":") return false;
          offset += 1;
        }

        if (!parseValue(containerDepth + 1)) return false;
        skipWhitespace();
        if (source[offset] === closing) {
          offset += 1;
          return true;
        }
        if (source[offset] !== ",") return false;
        offset += 1;
        skipWhitespace();
      }
      return false;
    }

    for (const literal of ["true", "false", "null"]) {
      if (source.startsWith(literal, offset)) {
        offset += literal.length;
        return true;
      }
    }
    return parseNumberToken();
  };

  if (!parseValue(0)) return false;
  skipWhitespace();
  return offset === source.length;
}

/** Decode bounded schema-v1 report bytes for display only; this is not host verification. */
export function decodeAgentReport(bytes: Uint8Array): CodingAgentAgentReport | null {
  if (bytes.byteLength > MAX_REPORT_BYTES) return null;
  try {
    const decoded = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    if (!hasUniqueJsonObjectKeys(decoded)) return null;
    const value: unknown = JSON.parse(decoded);
    return isAgentReport(value) ? value : null;
  } catch {
    return null;
  }
}
