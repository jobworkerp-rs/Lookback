import { invoke } from "@tauri-apps/api/core";

export interface NativeTerminalProfile {
  profileId: string;
  label: string;
  imageUri: string;
  expectedCliVersion: string;
  allowedBaseBranches: string[];
}

export interface NativeTerminalStatus {
  available: boolean;
  blockers: string[];
  profiles: NativeTerminalProfile[];
}

export interface NativeTerminalPrepareRequest {
  issueId: string;
  profileId: string;
  baseBranch: string;
  prompt: string | null;
}

export interface NativeTerminalStartRequest {
  previewId: string;
  previewDigest: string;
}

export interface NativeTerminalPreview {
  previewId: string;
  previewDigest: string;
  sessionId: string;
  profileId: string;
  expectedSettingsRevision: string;
  expectedIssueRevision: string;
  repository: {
    host: string;
    organization: string;
    repository: string;
  };
  baseBranch: string;
  baseCommit: string;
  command: string;
  args: string[];
  workingDirectory: string;
  initialPrompt: string;
  expectedCliVersion: string;
  safeImageDigest: string;
  runtimeObserved: boolean;
}

export type NativeTerminalSessionState =
  | "starting"
  | "running"
  | "closed"
  | "cancelled"
  | "unknown";

export interface NativeTerminalSession {
  sessionId: string;
  previewId: string;
  profileId: string;
  issueId: string;
  state: NativeTerminalSessionState;
  jobId: string | null;
  exitCode: number | null;
  endReceived: boolean;
  streamEof: boolean;
  streamError: string | null;
  cancelRequested: boolean;
}

export type NativeTerminalEvent =
  | { kind: "output"; stream: string; data: number[] }
  | { kind: "exit"; exitCode: number; executionTimeMs: string; sandboxId: string }
  | { kind: "end" };

export interface NativeTerminalOutputRecord {
  sessionId: string;
  sequence: string;
  event: NativeTerminalEvent;
}

export interface NativeTerminalAttachment {
  session: NativeTerminalSession;
  events: NativeTerminalOutputRecord[];
  nextSequence: string;
  subscriptionId: string | null;
}

export interface NativeTerminalOutputPage {
  events: NativeTerminalOutputRecord[];
  nextSequence: string;
}

export type NativeTerminalInputDelivery = "accepted" | "unknown" | "unavailable";

export interface NativeTerminalOutputEvent {
  subscriptionId: string | null;
  record: NativeTerminalOutputRecord;
}

// JobId.value is protobuf int64; serialize the positive value as a decimal string.
const I64_MAX = 9_223_372_036_854_775_807n;
const U64_MAX = 18_446_744_073_709_551_615n;
const MAX_INITIAL_PROMPT_BYTES = 64 * 1024;

function asRecord(value: unknown, name: string): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`native terminal ${name} is malformed`);
  }
  return value as Record<string, unknown>;
}

function asString(value: unknown, name: string, allowEmpty = false): string {
  if (typeof value !== "string" || (!allowEmpty && value.length === 0)) {
    throw new Error(`native terminal ${name} is malformed`);
  }
  return value;
}

function asStringArray(value: unknown, name: string): string[] {
  if (!Array.isArray(value) || !value.every((entry) => typeof entry === "string")) {
    throw new Error(`native terminal ${name} is malformed`);
  }
  return value;
}

function asBoolean(value: unknown, name: string): boolean {
  if (typeof value !== "boolean") throw new Error(`native terminal ${name} is malformed`);
  return value;
}

function asSafeInteger(value: unknown, name: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value)) {
    throw new Error(`native terminal ${name} is malformed`);
  }
  return value;
}

function asUnsignedDecimal(value: unknown, name: string, maximum: bigint): string {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/.test(value)) {
    throw new Error(`native terminal ${name} is malformed`);
  }
  if (BigInt(value) > maximum) throw new Error(`native terminal ${name} is out of range`);
  return value;
}

function validateInitialPrompt(prompt: string | null): void {
  if (prompt === null) return;
  if (prompt.includes("\u0000")) {
    throw new Error("native terminal initial prompt cannot contain NUL");
  }
  if (new TextEncoder().encode(prompt).byteLength > MAX_INITIAL_PROMPT_BYTES) {
    throw new Error("native terminal initial prompt exceeds 64 KiB UTF-8");
  }
}

function asJobId(value: unknown): string | null {
  if (value === null) return null;
  const jobId = asUnsignedDecimal(value, "jobId", I64_MAX);
  if (jobId === "0") throw new Error("native terminal jobId must be positive");
  return jobId;
}

function parseProfile(value: unknown): NativeTerminalProfile {
  const profile = asRecord(value, "profile");
  return {
    profileId: asString(profile.profileId, "profileId"),
    label: asString(profile.label, "profile label"),
    imageUri: asString(profile.imageUri, "profile imageUri"),
    expectedCliVersion: asString(profile.expectedCliVersion, "expectedCliVersion"),
    allowedBaseBranches: asStringArray(profile.allowedBaseBranches, "allowedBaseBranches"),
  };
}

function parseStatus(value: unknown): NativeTerminalStatus {
  const status = asRecord(value, "status");
  if (!Array.isArray(status.profiles)) throw new Error("native terminal profiles is malformed");
  return {
    available: asBoolean(status.available, "available"),
    blockers: asStringArray(status.blockers, "blockers"),
    profiles: status.profiles.map(parseProfile),
  };
}

function parsePreview(value: unknown): NativeTerminalPreview {
  const preview = asRecord(value, "preview");
  if (typeof preview.runtimeObserved !== "boolean") {
    throw new Error("native terminal preview runtimeObserved is malformed");
  }
  const repository = asRecord(preview.repository, "repository");
  const initialPrompt = asString(preview.initialPrompt, "initialPrompt", true);
  validateInitialPrompt(initialPrompt);
  return {
    previewId: asString(preview.previewId, "previewId"),
    previewDigest: asString(preview.previewDigest, "previewDigest"),
    sessionId: asString(preview.sessionId, "sessionId"),
    profileId: asString(preview.profileId, "profileId"),
    expectedSettingsRevision: asUnsignedDecimal(
      preview.expectedSettingsRevision,
      "expectedSettingsRevision",
      I64_MAX,
    ),
    expectedIssueRevision: asUnsignedDecimal(
      preview.expectedIssueRevision,
      "expectedIssueRevision",
      I64_MAX,
    ),
    repository: {
      host: asString(repository.host, "repository host"),
      organization: asString(repository.organization, "repository organization"),
      repository: asString(repository.repository, "repository name"),
    },
    baseBranch: asString(preview.baseBranch, "baseBranch"),
    baseCommit: asString(preview.baseCommit, "baseCommit"),
    command: asString(preview.command, "command"),
    args: asStringArray(preview.args, "args"),
    workingDirectory: asString(preview.workingDirectory, "workingDirectory"),
    initialPrompt,
    expectedCliVersion: asString(preview.expectedCliVersion, "expectedCliVersion"),
    safeImageDigest: asString(preview.safeImageDigest, "safeImageDigest"),
    runtimeObserved: preview.runtimeObserved,
  };
}

function parseSession(value: unknown): NativeTerminalSession {
  const session = asRecord(value, "session");
  const state = session.state;
  if (
    typeof state !== "string" ||
    !["starting", "running", "closed", "cancelled", "unknown"].includes(state)
  ) {
    throw new Error("native terminal state is malformed");
  }
  const exitCode = session.exitCode === null ? null : asSafeInteger(session.exitCode, "exitCode");
  const streamError =
    session.streamError === null ? null : asString(session.streamError, "streamError", true);
  return {
    sessionId: asString(session.sessionId, "sessionId"),
    previewId: asString(session.previewId, "previewId"),
    profileId: asString(session.profileId, "profileId"),
    issueId: asString(session.issueId, "issueId"),
    state: state as NativeTerminalSessionState,
    jobId: asJobId(session.jobId),
    exitCode,
    endReceived: asBoolean(session.endReceived, "endReceived"),
    streamEof: asBoolean(session.streamEof, "streamEof"),
    streamError,
    cancelRequested: asBoolean(session.cancelRequested, "cancelRequested"),
  };
}

function parseOutputRecord(value: unknown): NativeTerminalOutputRecord {
  const record = asRecord(value, "output record");
  const event = asRecord(record.event, "output event");
  const kind = event.kind;
  let parsedEvent: NativeTerminalEvent;
  if (kind === "output") {
    if (
      !Array.isArray(event.data) ||
      !event.data.every((byte) => Number.isInteger(byte) && byte >= 0 && byte <= 255)
    ) {
      throw new Error("native terminal output data is malformed");
    }
    parsedEvent = {
      kind,
      stream: asString(event.stream, "output stream"),
      data: event.data as number[],
    };
  } else if (kind === "exit") {
    parsedEvent = {
      kind,
      exitCode: asSafeInteger(event.exitCode, "exitCode"),
      executionTimeMs: asUnsignedDecimal(event.executionTimeMs, "executionTimeMs", U64_MAX),
      sandboxId: asString(event.sandboxId, "sandboxId", true),
    };
  } else if (kind === "end") {
    parsedEvent = { kind };
  } else {
    throw new Error("native terminal output event kind is malformed");
  }
  return {
    sessionId: asString(record.sessionId, "output sessionId"),
    sequence: asUnsignedDecimal(record.sequence, "sequence", U64_MAX),
    event: parsedEvent,
  };
}

function parseOutputPage(value: unknown): NativeTerminalOutputPage {
  const page = asRecord(value, "output page");
  if (!Array.isArray(page.events)) throw new Error("native terminal output events are malformed");
  return {
    events: page.events.map(parseOutputRecord),
    nextSequence: asUnsignedDecimal(page.nextSequence, "nextSequence", U64_MAX),
  };
}

function parseAttachment(value: unknown): NativeTerminalAttachment {
  const attachment = asRecord(value, "attachment");
  const page = parseOutputPage(attachment);
  const subscriptionId =
    attachment.subscriptionId === null
      ? null
      : asString(attachment.subscriptionId, "subscriptionId");
  return {
    session: parseSession(attachment.session),
    events: page.events,
    nextSequence: page.nextSequence,
    subscriptionId,
  };
}

export async function nativeTerminalStatus(issueId: string): Promise<NativeTerminalStatus> {
  return parseStatus(await invoke<unknown>("native_terminal_status", { issueId }));
}

export async function nativeTerminalPrepare(
  request: NativeTerminalPrepareRequest,
): Promise<NativeTerminalPreview> {
  validateInitialPrompt(request.prompt);
  return parsePreview(await invoke<unknown>("native_terminal_prepare", { request }));
}

export async function nativeTerminalStart(
  request: NativeTerminalStartRequest,
): Promise<NativeTerminalSession> {
  return parseSession(await invoke<unknown>("native_terminal_start", { request }));
}

export async function nativeTerminalCancel(sessionId: string): Promise<NativeTerminalSession> {
  const session = parseSession(
    await invoke<unknown>("native_terminal_cancel", { request: { sessionId } }),
  );
  if (session.sessionId !== sessionId) {
    throw new Error("native terminal cancel returned a different sessionId");
  }
  return session;
}

/** Detaches an event receiver only; the native process/session remains owned by its worker. */
export async function nativeTerminalDetach(request: {
  sessionId: string;
  subscriptionId: string;
}): Promise<void> {
  await invoke<void>("native_terminal_detach", { request });
}

export async function nativeTerminalList(issueId: string): Promise<NativeTerminalSession[]> {
  const sessions = await invoke<unknown>("native_terminal_list", { issueId });
  if (!Array.isArray(sessions)) throw new Error("native terminal session list is malformed");
  return sessions.map(parseSession);
}

export async function nativeTerminalAttach(request: {
  sessionId: string;
  afterSequence: string;
}): Promise<NativeTerminalAttachment> {
  asUnsignedDecimal(request.afterSequence, "afterSequence", U64_MAX);
  return parseAttachment(await invoke<unknown>("native_terminal_attach", { request }));
}

export async function nativeTerminalOutput(request: {
  sessionId: string;
  afterSequence: string;
}): Promise<NativeTerminalOutputPage> {
  asUnsignedDecimal(request.afterSequence, "afterSequence", U64_MAX);
  return parseOutputPage(await invoke<unknown>("native_terminal_output", { request }));
}

export async function nativeTerminalSendInput(
  sessionId: string,
  bytes: Uint8Array,
): Promise<NativeTerminalInputDelivery> {
  const result = await invoke<unknown>("native_terminal_send_input", {
    request: { sessionId, bytes: Array.from(bytes) },
  });
  if (result !== "accepted" && result !== "unknown" && result !== "unavailable") {
    throw new Error("native terminal input delivery is malformed");
  }
  return result;
}

export function parseNativeTerminalOutputEvent(value: unknown): NativeTerminalOutputEvent | null {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return null;
  const payload = value as Record<string, unknown>;
  const subscriptionId =
    payload.subscriptionId === undefined || payload.subscriptionId === null
      ? null
      : typeof payload.subscriptionId === "string"
        ? payload.subscriptionId
        : undefined;
  if (subscriptionId === undefined) return null;
  try {
    return {
      subscriptionId,
      record: parseOutputRecord(payload.record ?? payload),
    };
  } catch {
    return null;
  }
}
