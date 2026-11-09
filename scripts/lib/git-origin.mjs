/**
 * Normalise a Git remote URL into the credential-free form recorded in
 * provenance manifests. scp-style remotes (`git@host:owner/repo.git`) become
 * `ssh://host/owner/repo.git` because the manifests and the native bootstrap
 * accept only `https://` and `ssh://` URLs. Usernames, passwords, query strings,
 * and fragments are removed so no credential or branch selector leaks.
 *
 * @param {string} raw remote URL as reported by `git remote get-url`
 * @param {string} label subject used in the thrown error message
 * @returns {string}
 */
export function credentialFreeOrigin(raw, label = "Git origin") {
  const scp = raw.match(/^(?:[^@/]+@)?([^:/]+):(.+)$/);
  if (scp && !raw.includes("://")) return `ssh://${scp[1]}/${scp[2]}`;
  let parsed;
  try {
    parsed = new URL(raw);
  } catch {
    throw new Error(`${label} must be a remote HTTPS or SSH URL`);
  }
  if (!["https:", "http:", "ssh:"].includes(parsed.protocol) || !parsed.hostname) {
    throw new Error(`${label} must be a remote HTTPS or SSH URL`);
  }
  parsed.username = "";
  parsed.password = "";
  parsed.search = "";
  parsed.hash = "";
  return `${parsed.protocol}//${parsed.host}${parsed.pathname}`.replace(/\/$/, "");
}
