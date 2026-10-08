"use client";

import { useCallback, useMemo, type ReactNode } from "react";
import useSWR, { SWRConfig, useSWRConfig, type SWRConfiguration } from "swr";

import {
  listRepositories,
  type GithubAutomationErrorResponse,
  type GithubRepositoriesResponse,
} from "@/api-client";

export type RepositoryResourceState =
  | { status: "loading" }
  | { status: "ready"; value: GithubRepositoriesResponse }
  | { status: "error"; error: string; reason: string | null };

/** One cache entry shared by every repository consumer under `GithubRepositoriesProvider`. */
export const GITHUB_REPOSITORIES_KEY = "github-repositories";

/**
 * The list is fetched once and reused across consumers and client-side navigation. It is
 * refetched only when a consumer mounts with no cached list (first load or after a failure),
 * when `refresh()` is called, or after `useInvalidateGithubRepositories()` clears it.
 */
export const githubRepositoriesSwrOptions = {
  refreshInterval: 0,
  revalidateIfStale: false,
  revalidateOnFocus: false,
  revalidateOnReconnect: false,
  shouldRetryOnError: false,
} satisfies SWRConfiguration;

export class RepositoryListingError extends Error {
  constructor(readonly code: string, readonly reason: string | null) {
    super(code);
    this.name = "RepositoryListingError";
  }
}

export function githubRepositoriesQuery(refresh = false) {
  return { query: "", page: 1, perPage: 100, ...(refresh ? { refresh: true } : {}) };
}

export async function fetchGithubRepositories(
  { refresh = false }: Readonly<{ refresh?: boolean }> = {},
): Promise<GithubRepositoriesResponse> {
  let result: Awaited<ReturnType<typeof listRepositories>>;
  try {
    result = await listRepositories({
      cache: "no-store",
      credentials: "same-origin",
      query: githubRepositoriesQuery(refresh),
    });
  } catch {
    throw new RepositoryListingError("repository_query_failed", null);
  }
  if (result.data && result.response?.ok) return result.data;
  const problem = result.error as GithubAutomationErrorResponse | undefined;
  throw new RepositoryListingError(problem?.error ?? "repository_query_failed", problem?.reason ?? null);
}

export function repositoryResourceState({ data, error, isValidating }: Readonly<{
  data: GithubRepositoriesResponse | undefined;
  error: unknown;
  isValidating: boolean;
}>): RepositoryResourceState {
  if (error && !isValidating) {
    return error instanceof RepositoryListingError
      ? { status: "error", error: error.code, reason: error.reason }
      : { status: "error", error: "repository_query_failed", reason: null };
  }
  if (data) return { status: "ready", value: data };
  return { status: "loading" };
}

export function GithubRepositoriesProvider({ children }: Readonly<{ children: ReactNode }>) {
  return <SWRConfig value={{ provider: () => new Map() }}>{children}</SWRConfig>;
}

export function useGithubRepositories() {
  const { data, error, isValidating, mutate } = useSWR<GithubRepositoriesResponse, unknown>(
    GITHUB_REPOSITORIES_KEY,
    () => fetchGithubRepositories(),
    githubRepositoriesSwrOptions,
  );
  const state = useMemo(() => repositoryResourceState({ data, error, isValidating }), [data, error, isValidating]);
  const refresh = useCallback(() => {
    void mutate(fetchGithubRepositories({ refresh: true }), { revalidate: false });
  }, [mutate]);
  return { refresh, state };
}

/** Drops the cached list without fetching; the next consumer to mount loads it again. */
export function useInvalidateGithubRepositories() {
  const { mutate } = useSWRConfig();
  return useCallback(() => { void mutate(GITHUB_REPOSITORIES_KEY, undefined, { revalidate: false }); }, [mutate]);
}

export function RepositoryResource({ children, onRetry, state }: Readonly<{
  children: (value: GithubRepositoriesResponse) => ReactNode;
  onRetry: () => void;
  state: RepositoryResourceState;
}>) {
  if (state.status === "ready") return children(state.value);
  if (state.status === "loading") return <p className="text-sm text-muted-ink" role="status">Loading GitHub repositories…</p>;
  return <div className="space-y-2"><p className="text-sm text-err" role="alert">Repository listing failed: <code>{state.error}</code>{state.reason ? <> · <code>{state.reason}</code></> : null}</p><button className="rounded-control border px-3 py-2 text-sm font-semibold" onClick={onRetry} type="button">Retry repositories</button></div>;
}
