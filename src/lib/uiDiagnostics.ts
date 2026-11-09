import type { RootOptions } from "react-dom/client";

const DIAGNOSTIC_COMMAND = "report_ui_diagnostic";
const MAX_REPORTS_PER_SESSION = 10;
const MAX_LOCATIONS = 4;
const MAX_STACK_LINES = 80;
const MAX_STACK_LINE_LENGTH = 2_048;
const MAX_POSITION = 0xffff_ffff;

export type UiDiagnosticSource =
  | "react-caught"
  | "react-uncaught"
  | "window-error"
  | "unhandled-rejection";

const ERROR_KIND_NAMES = [
  "Error",
  "TypeError",
  "RangeError",
  "ReferenceError",
  "SyntaxError",
  "URIError",
  "EvalError",
  "AggregateError",
  "Unknown",
] as const;

export type UiDiagnosticErrorKind = (typeof ERROR_KIND_NAMES)[number];

export type UiDiagnosticCode = "maximum-update-depth" | "too-many-rerenders" | "unknown";

export type UiDiagnosticArea =
  | "entry"
  | "app"
  | "summaries"
  | "staged-progress-hook"
  | "staged-progress-panel"
  | "bundle"
  | "other";

export interface UiDiagnosticLocation {
  area: UiDiagnosticArea;
  line: number;
  column: number;
}

export interface UiDiagnostic {
  source: UiDiagnosticSource;
  error_kind: UiDiagnosticErrorKind;
  code: UiDiagnosticCode;
  locations: UiDiagnosticLocation[];
}

type UiDiagnosticInvoke = (command: string, args: { diagnostic: UiDiagnostic }) => Promise<unknown>;

export interface UiDiagnosticsHooks {
  rootOptions: Pick<RootOptions, "onCaughtError" | "onUncaughtError">;
  dispose: () => void;
}

const ERROR_KINDS: ReadonlySet<string> = new Set(ERROR_KIND_NAMES);

function readStringProperty(value: unknown, key: string): string | undefined {
  if (!isObjectKey(value)) return undefined;
  try {
    const property = Reflect.get(value, key);
    return typeof property === "string" ? property : undefined;
  } catch {
    return undefined;
  }
}

function errorKind(error: unknown): UiDiagnosticErrorKind {
  const name = readStringProperty(error, "name");
  return name && ERROR_KINDS.has(name) ? (name as UiDiagnosticErrorKind) : "Unknown";
}

function diagnosticCode(error: unknown): UiDiagnosticCode {
  const message = readStringProperty(error, "message")?.slice(0, 1_024).toLowerCase();
  if (message?.includes("maximum update depth exceeded")) {
    return "maximum-update-depth";
  }
  if (message?.includes("too many re-renders")) {
    return "too-many-rerenders";
  }
  return "unknown";
}

const SOURCE_AREAS: readonly (readonly [string, UiDiagnosticArea])[] = [
  ["src/main.tsx", "entry"],
  ["src/App.tsx", "app"],
  ["src/pages/Summaries.tsx", "summaries"],
  ["src/hooks/useStagedProgress.ts", "staged-progress-hook"],
  ["src/components/StagedProgressPanel.tsx", "staged-progress-panel"],
];

function areaForLocation(rawLocation: string): UiDiagnosticArea {
  const path = (rawLocation.split(/[?#]/, 1)[0] ?? "").replaceAll("\\", "/");

  const source = SOURCE_AREAS.find(([file]) => path === file || path.endsWith(`/${file}`));
  if (source) return source[1];
  if (/(?:^|\/)assets\/[^/]+\.m?js$/.test(path)) return "bundle";
  return "other";
}

function locationsFromText(text: string | undefined): UiDiagnosticLocation[] {
  if (!text) return [];
  const found: UiDiagnosticLocation[] = [];
  const seen = new Set<string>();
  const lines = text.split("\n", MAX_STACK_LINES);

  for (const line of lines) {
    const match = /(?:^|[\s(@])([^\s()]+):(\d+):(\d+)\)?\s*$/.exec(
      line.slice(0, MAX_STACK_LINE_LENGTH),
    );
    if (!match) continue;
    const lineNumber = Number(match[2]);
    const column = Number(match[3]);
    if (
      !Number.isSafeInteger(lineNumber) ||
      !Number.isSafeInteger(column) ||
      lineNumber < 1 ||
      column < 1 ||
      lineNumber > MAX_POSITION ||
      column > MAX_POSITION
    ) {
      continue;
    }

    const location: UiDiagnosticLocation = {
      area: areaForLocation(match[1] ?? ""),
      line: lineNumber,
      column,
    };
    const signature = `${location.area}:${location.line}:${location.column}`;
    if (!seen.has(signature)) {
      seen.add(signature);
      found.push(location);
      if (found.length === MAX_LOCATIONS) break;
    }
  }

  return found;
}

function createDiagnostic(
  source: UiDiagnosticSource,
  error: unknown,
  componentStack?: string,
): UiDiagnostic {
  const stack = readStringProperty(error, "stack");
  const locations = [...locationsFromText(stack), ...locationsFromText(componentStack)];
  const uniqueLocations = locations.filter(
    (location, index) =>
      locations.findIndex(
        (candidate) =>
          candidate.area === location.area &&
          candidate.line === location.line &&
          candidate.column === location.column,
      ) === index,
  );

  return {
    source,
    error_kind: errorKind(error),
    code: diagnosticCode(error),
    locations: uniqueLocations.slice(0, MAX_LOCATIONS),
  };
}

function isObjectKey(value: unknown): value is object {
  return (typeof value === "object" && value !== null) || typeof value === "function";
}

export function installUiDiagnostics(
  target: Window,
  invoke: UiDiagnosticInvoke,
): UiDiagnosticsHooks {
  const reported = new Set<string>();
  const reactUncaughtErrors = new WeakSet<object>();
  let reportCount = 0;

  const report = (source: UiDiagnosticSource, error: unknown, componentStack?: string) => {
    if (reportCount >= MAX_REPORTS_PER_SESSION) return;
    const diagnostic = createDiagnostic(source, error, componentStack);
    const signature = JSON.stringify(diagnostic);
    if (reported.has(signature)) return;
    reported.add(signature);
    reportCount += 1;

    try {
      void Promise.resolve(invoke(DIAGNOSTIC_COMMAND, { diagnostic })).catch(() => undefined);
    } catch {
      // Diagnostics must not turn an application error into a second failure.
    }
  };

  const onWindowError: EventListener = (event) => {
    if (!(event instanceof ErrorEvent)) return;
    if (isObjectKey(event.error) && reactUncaughtErrors.delete(event.error)) return;
    report("window-error", event.error);
  };
  const onUnhandledRejection: EventListener = (event) => {
    report("unhandled-rejection", Reflect.get(event, "reason"));
  };

  target.addEventListener("error", onWindowError);
  target.addEventListener("unhandledrejection", onUnhandledRejection);

  const rootOptions: UiDiagnosticsHooks["rootOptions"] = {
    onCaughtError(error, errorInfo) {
      report("react-caught", error, errorInfo.componentStack);
      // React replaces its default callback when one is supplied. Keep its
      // console-visible error while the existing ErrorBoundary remains intact.
      console.error(error);
    },
    onUncaughtError(error, errorInfo) {
      report("react-uncaught", error, errorInfo.componentStack);
      // Preserve React's default global error reporting for this root.
      if (isObjectKey(error)) reactUncaughtErrors.add(error);
      if (typeof target.reportError === "function") {
        target.reportError(error);
      } else {
        target.setTimeout(() => {
          throw error;
        });
      }
    },
  };

  return {
    rootOptions,
    dispose() {
      target.removeEventListener("error", onWindowError);
      target.removeEventListener("unhandledrejection", onUnhandledRejection);
    },
  };
}
