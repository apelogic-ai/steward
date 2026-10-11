"use client";

import type { GithubRepositoryView } from "@/api-client";

const fieldClass = "min-h-11 w-full rounded-control border bg-panel px-3 font-normal";

type WorkspaceObject = Record<string, unknown>;

function record(value: unknown): WorkspaceObject | null {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as WorkspaceObject
    : null;
}

function workspaceEntries(value: string): unknown[] | null {
  try {
    const parsed = JSON.parse(value);
    return Array.isArray(parsed) ? parsed : null;
  } catch {
    return null;
  }
}

function firstGitEntry(entries: unknown[]): { index: number; entry: WorkspaceObject; git: WorkspaceObject } | null {
  for (const [index, value] of entries.entries()) {
    const entry = record(value);
    const git = record(entry?.git);
    if (entry && git) return { index, entry, git };
  }
  return null;
}

function normalizedRepositoryName(value: string): string {
  const trimmed = value.trim().replace(/\.git$/i, "");
  try {
    const url = new URL(trimmed);
    const path = url.pathname.replace(/^\/+|\/+$/g, "");
    return path.toLocaleLowerCase("en-US");
  } catch {
    return trimmed.replace(/^\/+|\/+$/g, "").toLocaleLowerCase("en-US");
  }
}

export function workspaceRepositoryName(repository: Pick<GithubRepositoryView, "name" | "owner">): string {
  return `${repository.owner}/${repository.name}`;
}

export function canonicalWorkspaceRef(value: string): string {
  const trimmed = value.trim();
  return trimmed.startsWith("git:ref:") ? trimmed : `git:ref:${trimmed}`;
}

export function readableWorkspaceRef(value: unknown): string | null {
  if (typeof value !== "string") return null;
  if (value.startsWith("git:ref:")) return value.slice("git:ref:".length);
  if (value === "trigger" || value.startsWith("git:sha1:")) return null;
  return value;
}

export function matchingWorkspaceRepository(
  value: unknown,
  repositories: readonly GithubRepositoryView[],
): GithubRepositoryView | null {
  if (typeof value !== "string") return null;
  const normalized = normalizedRepositoryName(value);
  return repositories.find((repository) => (
    normalizedRepositoryName(workspaceRepositoryName(repository)) === normalized
      || normalizedRepositoryName(repository.url) === normalized
  )) ?? null;
}

export function updateWorkspaceRepository(
  value: string,
  repository: Pick<GithubRepositoryView, "name" | "owner">,
  reference: string,
): string | null {
  const entries = workspaceEntries(value);
  if (!entries || !reference.trim()) return null;
  const selection = firstGitEntry(entries);
  const git = {
    ...(selection?.git ?? {}),
    repository: workspaceRepositoryName(repository),
    ref: canonicalWorkspaceRef(reference),
  };
  const next = [...entries];
  if (selection) next[selection.index] = { ...selection.entry, git };
  else next.push({ git });
  return JSON.stringify(next, null, 2);
}

export function WorkspaceRepositoryControls({
  onChange,
  repositories,
  value,
}: Readonly<{
  onChange: (value: string) => void;
  repositories: readonly GithubRepositoryView[];
  value: string;
}>) {
  const entries = workspaceEntries(value);
  const selection = entries ? firstGitEntry(entries) : null;
  const matched = matchingWorkspaceRepository(selection?.git.repository, repositories);
  const currentReference = readableWorkspaceRef(selection?.git.ref);
  const reference = currentReference ?? matched?.defaultBranch ?? "";
  const preservesLegacySelector = Boolean(selection && (!matched || !currentReference));

  function selectRepository(repositoryId: string) {
    const repository = repositories.find((candidate) => candidate.repositoryId === repositoryId);
    if (!repository) return;
    const next = updateWorkspaceRepository(value, repository, currentReference ?? repository.defaultBranch);
    if (next) onChange(next);
  }

  function selectReference(nextReference: string) {
    if (!matched) return;
    const next = updateWorkspaceRepository(value, matched, nextReference);
    if (next) onChange(next);
  }

  return <div className="grid gap-4 md:grid-cols-2">
    <label className="grid gap-2 text-sm font-semibold">Workspace repository
      <select className={fieldClass} disabled={!entries} onChange={(event) => selectRepository(event.target.value)} value={matched?.repositoryId ?? ""}>
        <option value="">{selection ? "Keep existing selector" : "No repository checkout"}</option>
        {repositories.map((repository) => <option disabled={!repository.ready} key={repository.repositoryId} value={repository.repositoryId}>{workspaceRepositoryName(repository)}{repository.ready ? "" : " · not approved"}</option>)}
      </select>
    </label>
    <label className="grid gap-2 text-sm font-semibold">Branch or tag
      <input className={`${fieldClass} font-mono`} disabled={!matched} onChange={(event) => selectReference(event.target.value)} placeholder={matched?.defaultBranch ?? "Choose a repository first"} value={reference} />
    </label>
    {!entries ? <p className="text-sm text-err md:col-span-2" role="alert">Workspace must be a valid JSON array before using the repository picker.</p> : null}
    {preservesLegacySelector ? <p className="text-xs text-muted-ink md:col-span-2">The existing repository and revision selection remains unchanged until you choose a repository and branch or tag.</p> : null}
  </div>;
}
