/**
 * Build-time access to the public GitHub REST API.
 *
 * Responsibilities:
 * - One cached request per URL per build: every page of a multi-page build
 *   shares the same promise, so the API is hit once however many pages ask.
 * - Never fail the build: a network error, timeout or non-2xx status resolves
 *   to `null` and callers render their offline fallback.
 * - Keep the token server-side (see {@link githubJson}).
 */

/** The repository the site describes, as `owner/name`. */
export const REPO = "BankkRoll/STRATA";
/** The repository's page on GitHub. */
export const REPO_URL = `https://github.com/${REPO}`;

const API_ORIGIN = "https://api.github.com";
const cache = new Map<string, Promise<unknown>>();

/**
 * Fetches and parses a JSON document from api.github.com, once per build.
 *
 * SECURITY: `GITHUB_TOKEN`, when set (the Pages workflow sets it to lift the
 * anonymous rate limit), is attached only to requests whose origin is exactly
 * api.github.com. It is never written into any page.
 *
 * @param path - API path starting with `/`, e.g. `/repos/owner/name`.
 * @returns The parsed body, or `null` when GitHub could not be reached.
 * @example
 * const repo = await githubJson<{ stargazers_count: number }>(`/repos/${REPO}`);
 */
export function githubJson<T>(path: string): Promise<T | null> {
  const url = new URL(path, API_ORIGIN);
  const key = url.href;
  let hit = cache.get(key) as Promise<T | null> | undefined;
  if (!hit) {
    hit = request<T>(url);
    cache.set(key, hit);
  }
  return hit;
}

async function request<T>(url: URL): Promise<T | null> {
  const headers: Record<string, string> = { Accept: "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28" };
  const token = process.env.GITHUB_TOKEN;
  if (token && url.origin === API_ORIGIN) headers.Authorization = `Bearer ${token}`;
  try {
    const res = await fetch(url, { headers, signal: AbortSignal.timeout(15_000) });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return (await res.json()) as T;
  } catch (e) {
    console.warn(`github: ${url.pathname} unavailable, using the offline fallback (${String(e)})`);
    return null;
  }
}

/** Repository figures shown in the page chrome. */
export interface RepoStats {
  stars: number;
  forks: number;
}

/**
 * Loads the repository's star and fork counts.
 *
 * @returns The counts, or `null` when GitHub could not be reached.
 */
export async function loadRepoStats(): Promise<RepoStats | null> {
  const repo = await githubJson<{ stargazers_count?: unknown; forks_count?: unknown }>(`/repos/${REPO}`);
  if (!repo || typeof repo.stargazers_count !== "number") return null;
  return { stars: repo.stargazers_count, forks: typeof repo.forks_count === "number" ? repo.forks_count : 0 };
}

/**
 * Formats a count compactly for a button: `0`, `950`, `1.2k`, `12k`.
 *
 * @param n - A non-negative integer.
 * @returns The compact label.
 */
export function compactCount(n: number): string {
  if (n < 1000) return String(n);
  const k = n / 1000;
  return `${k < 10 ? k.toFixed(1).replace(/\.0$/, "") : Math.round(k)}k`;
}
