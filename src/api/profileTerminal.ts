import { invoke } from "@tauri-apps/api/core";
import { type NativeTerminalOutputPage, parseNativeTerminalOutputEvent } from "./nativeTerminal";

export interface ProfileTerminalSession {
  sessionId: string;
  profileId: string;
  profileVersion: number;
  settingsRevision: number;
  imageRef: string;
  state: "starting" | "running" | "cancelling" | "closed" | "unknown";
  jobId: string | null;
  exitCode: number | null;
  inputClosed: boolean;
}

const U64_MAX = 18_446_744_073_709_551_615n;
function sequence(value: unknown): bigint {
  if (
    typeof value !== "string" ||
    !/^(0|[1-9][0-9]*)$/.test(value) ||
    value.length > 20 ||
    BigInt(value) > U64_MAX
  )
    throw new Error("Invalid profile terminal sequence");
  return BigInt(value);
}
function parseSession(value: unknown): ProfileTerminalSession {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error("Invalid profile terminal session");
  const session = value as Record<string, unknown>;
  for (const key of ["sessionId", "profileId", "imageRef"]) {
    if (typeof session[key] !== "string" || !session[key] || session[key].length > 2048)
      throw new Error("Invalid profile terminal identity");
  }
  if (
    !Number.isSafeInteger(session.profileVersion) ||
    Number(session.profileVersion) <= 0 ||
    !Number.isSafeInteger(session.settingsRevision) ||
    Number(session.settingsRevision) <= 0 ||
    typeof session.inputClosed !== "boolean" ||
    !["starting", "running", "cancelling", "closed", "unknown"].includes(String(session.state)) ||
    (session.jobId !== null &&
      (typeof session.jobId !== "string" ||
        !/^[1-9][0-9]*$/.test(session.jobId) ||
        session.jobId.length > 19 ||
        BigInt(session.jobId) > 9_223_372_036_854_775_807n)) ||
    (session.exitCode !== null &&
      (!Number.isInteger(session.exitCode) ||
        Number(session.exitCode) < -2_147_483_648 ||
        Number(session.exitCode) > 2_147_483_647)) ||
    (session.state === "closed" &&
      (session.exitCode === null || !session.inputClosed || session.jobId === null)) ||
    (session.state === "running" && session.jobId === null)
  )
    throw new Error("Invalid profile terminal facts");
  return value as ProfileTerminalSession;
}

export async function profileTerminalStart(request: {
  expectedRevision: number;
  profileId: string;
}): Promise<ProfileTerminalSession> {
  return parseSession(await invoke("coding_agent_profile_terminal_start", { request }));
}
export async function profileTerminalList(): Promise<ProfileTerminalSession[]> {
  const result = await invoke<unknown>("coding_agent_profile_terminal_list");
  if (!Array.isArray(result) || result.length > 128)
    throw new Error("Invalid profile terminal list");
  return result.map(parseSession);
}
export async function profileTerminalSession(sessionId: string): Promise<ProfileTerminalSession> {
  const session = parseSession(
    await invoke("coding_agent_profile_terminal_session", { request: { sessionId } }),
  );
  if (session.sessionId !== sessionId) throw new Error("Profile terminal session changed");
  return session;
}
export async function profileTerminalOutput(
  sessionId: string,
  afterSequence: string,
): Promise<NativeTerminalOutputPage> {
  let cursor = sequence(afterSequence);
  const result = await invoke<NativeTerminalOutputPage>("coding_agent_profile_terminal_output", {
    request: { sessionId, afterSequence },
  });
  if (!result || !Array.isArray(result.events) || result.events.length > 16)
    throw new Error("Invalid profile terminal output");
  const events = result.events.map((value) => {
    const parsed = parseNativeTerminalOutputEvent({ record: value });
    if (
      !parsed ||
      parsed.record.sessionId !== sessionId ||
      sequence(parsed.record.sequence) !== cursor + 1n
    )
      throw new Error("Profile terminal output is unavailable");
    cursor = sequence(parsed.record.sequence);
    return parsed.record;
  });
  if (sequence(result.nextSequence) !== cursor) throw new Error("Invalid profile terminal cursor");
  return { events, nextSequence: result.nextSequence };
}
export async function profileTerminalSendInput(
  sessionId: string,
  bytes: Uint8Array,
): Promise<void> {
  if (!bytes.byteLength || bytes.byteLength > 64 * 1024)
    throw new Error("Invalid profile terminal input size");
  await invoke("coding_agent_profile_terminal_send_input", {
    request: { sessionId, bytes: Array.from(bytes) },
  });
}
export async function profileTerminalFinishInput(sessionId: string): Promise<void> {
  await invoke("coding_agent_profile_terminal_finish_input", { request: { sessionId } });
}
export async function profileTerminalCancel(sessionId: string): Promise<void> {
  await invoke("coding_agent_profile_terminal_cancel", { request: { sessionId } });
}
