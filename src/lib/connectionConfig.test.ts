import { describe, expect, it } from "vitest";
import type { ConnectionConfig } from "@/types/api";
import {
  connectionSignature,
  hasRemoteService,
  LOCAL_CONNECTION_CONFIG,
  missingRemoteUrl,
  modeDisabledReason,
  normalizeConnectionConfig,
} from "./connectionConfig";

const remote = (url: string) => ({ mode: "remote" as const, remote_url: url });

const ALL_REMOTE: ConnectionConfig = {
  ...LOCAL_CONNECTION_CONFIG,
  jobworkerp: remote("http://j:9000"),
  memories: remote("http://m:9010"),
  issue_tracker: {
    ...remote("http://t:9030"),
    remote_gateway_control_url: "http://t:9031",
    remote_gateway_worker_url: "http://t:9032",
  },
  conductor: remote("http://c:9020"),
};

describe("normalizeConnectionConfig", () => {
  it("trims values and turns blanks into null", () => {
    const normalized = normalizeConnectionConfig({
      ...LOCAL_CONNECTION_CONFIG,
      memories: { mode: "remote", remote_url: "  http://m:9010 " },
      worker_view: { ...LOCAL_CONNECTION_CONFIG.worker_view, memories_url: "   " },
      jobworkerp_host_paths: {
        ...LOCAL_CONNECTION_CONFIG.jobworkerp_host_paths,
        import_working_dir: " /srv/work ",
      },
    });
    expect(normalized.memories.remote_url).toBe("http://m:9010");
    expect(normalized.worker_view.memories_url).toBeNull();
    expect(normalized.jobworkerp_host_paths.import_working_dir).toBe("/srv/work");
  });

  it("keeps a local service's remote URL as a draft", () => {
    const normalized = normalizeConnectionConfig({
      ...LOCAL_CONNECTION_CONFIG,
      jobworkerp: { mode: "local", remote_url: "http://j:9000" },
    });
    expect(normalized.jobworkerp).toEqual({ mode: "local", remote_url: "http://j:9000" });
  });
});

describe("modeDisabledReason", () => {
  it("allows every choice from the all-local default except a remote jobworkerp or conductor", () => {
    const cfg = LOCAL_CONNECTION_CONFIG;
    expect(modeDisabledReason(cfg, "memories", "remote", true)).toBeNull();
    expect(modeDisabledReason(cfg, "issue_tracker", "remote", true)).toBeNull();
    expect(modeDisabledReason(cfg, "jobworkerp", "remote", true)).toBe(
      "settings.connection.constraint.jobworkerpNeedsRemote",
    );
    expect(modeDisabledReason(cfg, "conductor", "remote", true)).toBe(
      "settings.connection.constraint.conductorNeedsRemoteJobworkerp",
    );
  });

  it("allows a remote jobworkerp once memories and the tracker are remote", () => {
    const cfg = { ...ALL_REMOTE, jobworkerp: { mode: "local" as const, remote_url: null } };
    expect(modeDisabledReason(cfg, "jobworkerp", "remote", true)).toBeNull();
  });

  it("ignores the tracker while issue tracking is disabled", () => {
    const cfg = { ...LOCAL_CONNECTION_CONFIG, memories: remote("http://m:9010") };
    expect(modeDisabledReason(cfg, "jobworkerp", "remote", false)).toBeNull();
    expect(modeDisabledReason(cfg, "jobworkerp", "remote", true)).toBe(
      "settings.connection.constraint.jobworkerpNeedsRemote",
    );
  });

  it("keeps memories and the tracker remote while jobworkerp is remote", () => {
    expect(modeDisabledReason(ALL_REMOTE, "memories", "local", true)).toBe(
      "settings.connection.constraint.remoteJobworkerpNeedsRemote",
    );
    expect(modeDisabledReason(ALL_REMOTE, "issue_tracker", "local", true)).toBe(
      "settings.connection.constraint.remoteJobworkerpNeedsRemote",
    );
  });

  it("keeps jobworkerp remote while conductor is remote", () => {
    expect(modeDisabledReason(ALL_REMOTE, "jobworkerp", "local", true)).toBe(
      "settings.connection.constraint.remoteConductorNeedsRemoteJobworkerp",
    );
    const localConductor = {
      ...ALL_REMOTE,
      conductor: { mode: "local" as const, remote_url: null },
    };
    expect(modeDisabledReason(localConductor, "jobworkerp", "local", true)).toBeNull();
  });

  it("never disables the mode already selected", () => {
    const invalid = { ...LOCAL_CONNECTION_CONFIG, jobworkerp: remote("http://j:9000") };
    expect(modeDisabledReason(invalid, "jobworkerp", "remote", true)).toBeNull();
  });
});

describe("missingRemoteUrl", () => {
  it("requires URLs only for remote services", () => {
    expect(missingRemoteUrl(LOCAL_CONNECTION_CONFIG, true)).toBe(false);
    expect(missingRemoteUrl(ALL_REMOTE, true)).toBe(false);
    expect(missingRemoteUrl({ ...LOCAL_CONNECTION_CONFIG, memories: remote("  ") }, true)).toBe(
      true,
    );
  });

  it("requires the tracker's gateway URLs only while issue tracking is enabled", () => {
    const cfg = {
      ...ALL_REMOTE,
      issue_tracker: { ...ALL_REMOTE.issue_tracker, remote_gateway_worker_url: null },
    };
    expect(missingRemoteUrl(cfg, true)).toBe(true);
    expect(missingRemoteUrl(cfg, false)).toBe(false);
  });
});

describe("hasRemoteService", () => {
  it("reports whether any service leaves this machine", () => {
    expect(hasRemoteService(LOCAL_CONNECTION_CONFIG)).toBe(false);
    expect(hasRemoteService({ ...LOCAL_CONNECTION_CONFIG, conductor: remote("http://c:1") })).toBe(
      true,
    );
  });
});

describe("connectionSignature", () => {
  it("changes when any service's target changes", () => {
    const base = connectionSignature(LOCAL_CONNECTION_CONFIG);
    expect(connectionSignature({ ...LOCAL_CONNECTION_CONFIG })).toBe(base);
    for (const changed of [
      { ...LOCAL_CONNECTION_CONFIG, memories: remote("http://m:9010") },
      { ...LOCAL_CONNECTION_CONFIG, conductor: remote("http://c:9020") },
      {
        ...LOCAL_CONNECTION_CONFIG,
        issue_tracker: { ...LOCAL_CONNECTION_CONFIG.issue_tracker, mode: "remote" as const },
      },
    ]) {
      expect(connectionSignature(changed)).not.toBe(base);
    }
  });

  it("ignores a local service's draft URL", () => {
    expect(
      connectionSignature({
        ...LOCAL_CONNECTION_CONFIG,
        memories: { mode: "local", remote_url: "http://m:9010" },
      }),
    ).toBe(connectionSignature(LOCAL_CONNECTION_CONFIG));
  });
});
