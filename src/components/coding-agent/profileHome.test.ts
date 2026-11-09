import { describe, expect, it } from "vitest";
import type { CodingAgentProfile } from "@/types/api";
import { profileHomeReady } from "./profileHome";

const base = {
  containerHome: "/home/codespace/.agent-home",
  persistentMounts: [],
} as unknown as CodingAgentProfile;

describe("profileHomeReady", () => {
  it("requires a writable mount at or below the container home", () => {
    expect(profileHomeReady(base)).toBe(false);
    expect(
      profileHomeReady({
        ...base,
        persistentMounts: [
          {
            hostPath: "/h",
            guestPath: "/home/codespace/.agent-home",
            readOnly: true,
            executable: false,
          },
        ],
      }),
    ).toBe(false);
    expect(
      profileHomeReady({
        ...base,
        persistentMounts: [
          {
            hostPath: "/h",
            guestPath: "/home/codespace/.agent-home",
            readOnly: false,
            executable: false,
          },
        ],
      }),
    ).toBe(true);
    expect(
      profileHomeReady({
        ...base,
        persistentMounts: [
          {
            hostPath: "/h",
            guestPath: "/home/codespace/.agent-home/.codex",
            readOnly: false,
            executable: false,
          },
        ],
      }),
    ).toBe(true);
  });

  it("does not treat a sibling path with a shared prefix as the home", () => {
    expect(
      profileHomeReady({
        ...base,
        persistentMounts: [
          {
            hostPath: "/h",
            guestPath: "/home/codespace/.agent-home-other",
            readOnly: false,
            executable: false,
          },
        ],
      }),
    ).toBe(false);
  });

  it("treats host-shared profiles as ready because the backend prepares the shared home", () => {
    expect(
      profileHomeReady({
        ...base,
        hostConfiguration: {
          codexDirectory: "/home/me/.codex",
          claudeDirectory: null,
          claudeSettingsFile: null,
        },
      }),
    ).toBe(true);
  });
});
