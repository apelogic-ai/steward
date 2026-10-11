import { expect, test } from "bun:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";

import type { GithubRepositoryView } from "@/api-client";

import {
  canonicalWorkspaceRef,
  editWorkspaceReference,
  matchingWorkspaceRepository,
  readableWorkspaceRef,
  updateWorkspaceRepository,
  validWorkspaceBranchOrTag,
  WorkspaceRepositoryControls,
} from "./workspace-repository-controls";

const repository: GithubRepositoryView = {
  defaultBranch: "main",
  name: "repository",
  owner: "example-org",
  ownerId: "1000",
  private: true,
  ready: true,
  repositoryId: "1001",
  url: "https://github.com/example-org/repository.git",
};

test("the structured picker adds a readable repository and canonical named ref", () => {
  const updated = updateWorkspaceRepository("[]", repository, "release-v1");

  expect(JSON.parse(updated ?? "null")).toEqual([{
    git: {
      repository: "example-org/repository",
      ref: "git:ref:release-v1",
    },
  }]);
});

test("updating a named checkout preserves its other authority and sibling entries", () => {
  const initial = JSON.stringify([
    { git: { repository: "example-org/old", ref: "git:ref:main", history: { depth: 25 }, access: "copy" } },
    { scratch: { size: "2Gi" } },
  ]);
  const updated = updateWorkspaceRepository(initial, repository, "v1.2.3");

  expect(JSON.parse(updated ?? "null")).toEqual([
    { git: { repository: "example-org/repository", ref: "git:ref:v1.2.3", history: { depth: 25 }, access: "copy" } },
    { scratch: { size: "2Gi" } },
  ]);
});

test("existing stable-ID, self, trigger, and exact-commit forms remain distinguishable", () => {
  expect(matchingWorkspaceRepository({ ownerId: "1000", repositoryId: "1001" }, [repository])).toBeNull();
  expect(matchingWorkspaceRepository("self", [repository])).toBeNull();
  expect(readableWorkspaceRef("trigger")).toBeNull();
  expect(readableWorkspaceRef(`git:sha1:${"a".repeat(40)}`)).toBeNull();
  expect(readableWorkspaceRef("git:ref:main")).toBe("main");
  expect(canonicalWorkspaceRef("git:ref:main")).toBe("git:ref:main");
});

test("repository names and links select the admitted repository without changing its canonical label", () => {
  expect(matchingWorkspaceRepository("EXAMPLE-ORG/REPOSITORY.git", [repository])).toEqual(repository);
  expect(matchingWorkspaceRepository("https://github.com/example-org/repository.git", [repository])).toEqual(repository);
  expect(matchingWorkspaceRepository("https://gitlab.com/example-org/repository.git", [repository])).toBeNull();
});

test("clearing and retyping a ref keeps the empty draft without rewriting the workspace", () => {
  const initial = JSON.stringify([{ git: { repository: "example-org/repository", ref: "git:ref:main" } }]);
  const cleared = editWorkspaceReference(initial, repository, "");

  expect(cleared).toEqual({ draft: "", workspace: null });
  const retyped = editWorkspaceReference(initial, repository, "release-v1");
  expect(retyped.draft).toBe("release-v1");
  expect(JSON.parse(retyped.workspace ?? "null")[0].git.ref).toBe("git:ref:release-v1");
});

test("the branch or tag field rejects trigger and exact-commit selectors", () => {
  expect(validWorkspaceBranchOrTag("main")).toBeTrue();
  expect(validWorkspaceBranchOrTag("git:ref:main")).toBeTrue();
  expect(validWorkspaceBranchOrTag("trigger")).toBeFalse();
  expect(validWorkspaceBranchOrTag(`git:sha1:${"a".repeat(40)}`)).toBeFalse();
});

test("the Task editor control offers admitted repositories and preserves a legacy selector", () => {
  const html = renderToStaticMarkup(createElement(WorkspaceRepositoryControls, {
    onChange: () => undefined,
    repositories: [repository],
    value: JSON.stringify([{
      git: {
        repository: { ownerId: "1000", repositoryId: "1001" },
        ref: `git:sha1:${"a".repeat(40)}`,
      },
    }]),
  }));

  expect(html).toContain("Workspace repository");
  expect(html).toContain("Branch or tag");
  expect(html).toContain("example-org/repository");
  expect(html).toContain("repository and revision selection remains unchanged");
});
