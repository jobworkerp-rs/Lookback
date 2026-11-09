import type { ConnectionConfig, ConnectionMode, EndpointConfig } from "@/types/api";

/** The services whose target is chosen independently. */
export const CONNECTION_SERVICES = [
  "jobworkerp",
  "memories",
  "issue_tracker",
  "conductor",
] as const;
export type ConnectionService = (typeof CONNECTION_SERVICES)[number];

export const LOCAL_CONNECTION_CONFIG: ConnectionConfig = {
  jobworkerp: { mode: "local", remote_url: null },
  memories: { mode: "local", remote_url: null },
  issue_tracker: {
    mode: "local",
    remote_url: null,
    remote_gateway_control_url: null,
    remote_gateway_worker_url: null,
  },
  conductor: { mode: "local", remote_url: null },
  worker_view: {
    jobworkerp_url: null,
    memories_url: null,
    issue_tracker_url: null,
    issue_tracker_gateway_worker_url: null,
  },
  jobworkerp_host_paths: {
    memories_import_bin: null,
    opencode_db_path: null,
    import_working_dir: null,
  },
};

function blankToNull(value: string | null | undefined): string | null {
  const trimmed = value?.trim() ?? "";
  return trimmed === "" ? null : trimmed;
}

function normalizeStrings<T extends Record<string, string | null>>(record: T): T {
  return Object.fromEntries(
    Object.entries(record).map(([key, value]) => [key, blankToNull(value)]),
  ) as T;
}

function normalizeEndpoint<T extends EndpointConfig>({ mode, ...urls }: T): T {
  // Every field of an endpoint besides `mode` is a URL.
  return { ...normalizeStrings(urls as Record<string, string | null>), mode } as T;
}

/** The shape saved and compared: trimmed, with blanks as `null`. A local
 *  service keeps its remote URL as a draft, as the backend does. */
export function normalizeConnectionConfig(cfg: ConnectionConfig): ConnectionConfig {
  return {
    jobworkerp: normalizeEndpoint(cfg.jobworkerp),
    memories: normalizeEndpoint(cfg.memories),
    issue_tracker: normalizeEndpoint(cfg.issue_tracker),
    conductor: normalizeEndpoint(cfg.conductor),
    worker_view: normalizeStrings({ ...cfg.worker_view }),
    jobworkerp_host_paths: normalizeStrings({ ...cfg.jobworkerp_host_paths }),
  };
}

const isRemote = (endpoint: EndpointConfig) => endpoint.mode === "remote";

/**
 * Why `mode` cannot be chosen for `service` given the rest of `cfg`, as an
 * i18n key; `null` when it can. Mirrors the backend's combination rules: a
 * remote jobworkerp cannot reach a local memories or tracker, and a remote
 * conductor cannot enqueue to a local jobworkerp. The current mode is never
 * disabled, so an invalid saved combination can still be repaired.
 */
export function modeDisabledReason(
  cfg: ConnectionConfig,
  service: ConnectionService,
  mode: ConnectionMode,
  issueTrackingEnabled: boolean,
): string | null {
  if (cfg[service].mode === mode) return null;
  if (mode === "remote") {
    if (service === "jobworkerp") {
      const trackerLocal = issueTrackingEnabled && !isRemote(cfg.issue_tracker);
      return !isRemote(cfg.memories) || trackerLocal
        ? "settings.connection.constraint.jobworkerpNeedsRemote"
        : null;
    }
    if (service === "conductor" && !isRemote(cfg.jobworkerp)) {
      return "settings.connection.constraint.conductorNeedsRemoteJobworkerp";
    }
    return null;
  }
  if (service === "jobworkerp") {
    return isRemote(cfg.conductor)
      ? "settings.connection.constraint.remoteConductorNeedsRemoteJobworkerp"
      : null;
  }
  if ((service === "memories" || service === "issue_tracker") && isRemote(cfg.jobworkerp)) {
    return "settings.connection.constraint.remoteJobworkerpNeedsRemote";
  }
  return null;
}

/** Whether a remote service still lacks a URL the backend requires. */
export function missingRemoteUrl(cfg: ConnectionConfig, issueTrackingEnabled: boolean): boolean {
  const required: (string | null)[] = [];
  for (const endpoint of [cfg.jobworkerp, cfg.memories, cfg.conductor]) {
    if (isRemote(endpoint)) required.push(endpoint.remote_url);
  }
  const tracker = cfg.issue_tracker;
  if (issueTrackingEnabled && isRemote(tracker)) {
    required.push(
      tracker.remote_url,
      tracker.remote_gateway_control_url,
      tracker.remote_gateway_worker_url,
    );
  }
  return required.some((url) => blankToNull(url) == null);
}

export function hasRemoteService(cfg: ConnectionConfig): boolean {
  return CONNECTION_SERVICES.some((service) => isRemote(cfg[service]));
}

/** Identifies the active targets; a local service's draft URL is ignored. */
export function connectionSignature(cfg: ConnectionConfig): string {
  return CONNECTION_SERVICES.map((service) => {
    const endpoint = cfg[service];
    return isRemote(endpoint) ? `remote:${endpoint.remote_url ?? ""}` : "local";
  }).join("|");
}
