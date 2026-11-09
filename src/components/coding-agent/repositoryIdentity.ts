import type { CodingAgentRepositoryIdentity } from "@/types/api";

/** The `host/organization/repository` form shown in the UI and used as a key. */
export function repositoryIdentityLabel(repository: CodingAgentRepositoryIdentity): string {
  return `${repository.host}/${repository.organization}/${repository.repository}`;
}

export function sameRepositoryIdentity(
  left: CodingAgentRepositoryIdentity,
  right: CodingAgentRepositoryIdentity,
): boolean {
  return (
    left.host === right.host &&
    left.organization === right.organization &&
    left.repository === right.repository
  );
}
