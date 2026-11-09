import { describe, expect, it } from "vitest";
import { repositoryIdentityLabel, sameRepositoryIdentity } from "./repositoryIdentity";

const identity = { host: "forge.example", organization: "team", repository: "app" };

describe("repository identity helpers", () => {
  it("labels an identity as host/organization/repository", () => {
    expect(repositoryIdentityLabel(identity)).toBe("forge.example/team/app");
  });

  it("compares every identity part", () => {
    expect(sameRepositoryIdentity(identity, { ...identity })).toBe(true);
    for (const part of ["host", "organization", "repository"] as const) {
      expect(sameRepositoryIdentity(identity, { ...identity, [part]: "other" })).toBe(false);
    }
  });
});
