"use client";

import { useState } from "react";

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

function repositorySelector(value: string): { host: string | null; path: string } {
  const trimmed = value.trim().replace(/\.git$/i, "");
  try {
    const url = new URL(trimmed);
    const path = url.pathname.replace(/^\/+|\/+$/g, "");
    return {
      host: url.host.toLocaleLowerCase("en-US"),
      path: path.toLocaleLowerCase("en-US"),
    };
  } catch {
    return {
      host: null,
      path: trimmed.replace(/^\/+|\/+$/g, "").toLocaleLowerCase("en-US"),
    };
  }
}

export function workspaceRepositoryName(repository: Pick<GithubRepositoryView, "name" | "owner">): string {
  return `${repository.owner}/${repository.name}`;
}

export function canonicalWorkspaceRef(value: string): string {
  const trimmed = value.trim();
  return trimmed.startsWith("git:ref:") ? trimmed : `git:ref:${trimmed}`;
}

export function validWorkspaceBranchOrTag(value: string): boolean {
  const trimmed = value.trim();
  if (trimmed === "trigger") return false;
  const explicit = trimmed.startsWith("git:ref:");
  if (trimmed.startsWith("git:") && !explicit) return false;
  const reference = explicit ? trimmed.slice("git:ref:".length) : trimmed;
  if (!reference || new TextEncoder().encode(reference).byteLength > 512 || reference.startsWith("-") || reference.endsWith("/")) return false;
  if (!explicit && (reference === "HEAD" || (/^[0-9a-f]{7,40}$/i).test(reference))) return false;
  return !reference.includes("..")
    && !reference.includes("@{")
    && !reference.includes("\\")
    && !(/[\u0000-\u0020\u007f-\u009f]/u).test(reference);
}

export function readableWorkspaceRef(value: unknown): string | null {
  if (typeof value !== "string") return null;
  if (value.startsWith("git:ref:")) return value.slice("git:ref:".length);
  return value;
}

export function matchingWorkspaceRepository(
  value: unknown,
  repositories: readonly GithubRepositoryView[],
): GithubRepositoryView | null {
  if (typeof value !== "string") return null;
  const requested = repositorySelector(value);
  return repositories.find((repository) => {
    const name = repositorySelector(workspaceRepositoryName(repository));
    if (requested.host === null) return requested.path === name.path;
    const link = repositorySelector(repository.url);
    return requested.host === link.host && requested.path === link.path;
  }) ?? null;
}

export function updateWorkspaceRepository(
  value: string,
  repository: Pick<GithubRepositoryView, "name" | "owner">,
  reference: string,
): string | null {
  const entries = workspaceEntries(value);
  if (!entries || !validWorkspaceBranchOrTag(reference)) return null;
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

export function editWorkspaceReference(
  value: string,
  repository: Pick<GithubRepositoryView, "name" | "owner">,
  draft: string,
): { draft: string; workspace: string | null } {
  return {
    draft,
    workspace: updateWorkspaceRepository(value, repository, draft),
  };
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
  const derivedReference = currentReference ?? matched?.defaultBranch ?? "";
  const [referenceEdit, setReferenceEdit] = useState(() => ({
    source: value,
    draft: derivedReference,
  }));
  const reference = referenceEdit.source === value ? referenceEdit.draft : derivedReference;
  const preservesLegacySelector = Boolean(selection && (!matched || !validWorkspaceBranchOrTag(reference)));

  function selectRepository(repositoryId: string) {
    const repository = repositories.find((candidate) => candidate.repositoryId === repositoryId);
    if (!repository) return;
    const draft = currentReference && validWorkspaceBranchOrTag(currentReference)
      ? currentReference
      : repository.defaultBranch;
    const next = updateWorkspaceRepository(value, repository, draft);
    if (next) {
      setReferenceEdit({ source: next, draft });
      onChange(next);
    }
  }

  function selectReference(nextReference: string) {
    if (!matched) return;
    const edit = editWorkspaceReference(value, matched, nextReference);
    setReferenceEdit({ source: edit.workspace ?? value, draft: edit.draft });
    if (edit.workspace) onChange(edit.workspace);
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
    {matched && !validWorkspaceBranchOrTag(reference) ? <p className="text-sm text-err md:col-span-2" role="alert">Workspace entry {workspaceRepositoryName(matched)} accepts trigger, a full git:sha1 commit, or a branch or tag in JSON. Enter a branch or tag other than HEAD or a commit ID in this field.</p> : null}
    {preservesLegacySelector ? <p className="text-xs text-muted-ink md:col-span-2">The existing repository and revision selection remains unchanged until you choose a repository and branch or tag.</p> : null}
  </div>;
}
