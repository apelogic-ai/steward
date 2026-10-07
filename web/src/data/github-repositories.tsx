"use client";

import { useEffect, useState, type ReactNode } from "react";

import {
  listRepositories,
  type GithubAutomationErrorResponse,
  type GithubRepositoriesResponse,
} from "@/api-client";

export type RepositoryResourceState =
  | { status: "loading" }
  | { status: "ready"; value: GithubRepositoriesResponse }
  | { status: "error"; error: string; reason: string | null };

function repositoryFailure(problem: GithubAutomationErrorResponse | undefined): Pick<Extract<RepositoryResourceState, { status: "error" }>, "error" | "reason"> {
  return {
    error: problem?.error ?? "repository_query_failed",
    reason: problem?.reason ?? null,
  };
}

export function useGithubRepositories() {
  const [attempt, setAttempt] = useState(0);
  const [state, setState] = useState<RepositoryResourceState>({ status: "loading" });
  useEffect(() => {
    let active = true;
    void listRepositories({
      cache: "no-store",
      credentials: "same-origin",
      query: { query: "", page: 1, perPage: 100 },
    }).then((result) => {
      if (!active) return;
      if (result.data && result.response?.ok) {
        setState({ status: "ready", value: result.data });
        return;
      }
      setState({ status: "error", ...repositoryFailure(result.error as GithubAutomationErrorResponse | undefined) });
    }).catch(() => {
      if (active) setState({ status: "error", error: "repository_query_failed", reason: null });
    });
    return () => { active = false; };
  }, [attempt]);
  return {
    retry: () => {
      setState({ status: "loading" });
      setAttempt((value) => value + 1);
    },
    state,
  };
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
