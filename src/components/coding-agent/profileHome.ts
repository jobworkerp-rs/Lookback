import type { CodingAgentProfile } from "@/types/api";

function isSameOrBelow(guestPath: string, containerHome: string): boolean {
  const home = containerHome.replace(/\/+$/, "");
  return guestPath === home || guestPath.startsWith(`${home}/`);
}

/**
 * Whether the authentication/settings shell can persist CLI state for this
 * profile. The backend refuses the shell unless the container home is backed by
 * a writable mount; host-shared profiles get their shared home from the backend
 * itself, so no explicit mount is needed there.
 */
export function profileHomeReady(profile: CodingAgentProfile): boolean {
  if (profile.hostConfiguration) return true;
  return profile.persistentMounts.some(
    (mount) => !mount.readOnly && isSameOrBelow(mount.guestPath, profile.containerHome),
  );
}
