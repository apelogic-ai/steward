import { expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { GithubRepositoryView } from "@/api-client";

import {
  canonicalWorkspaceRef,
  matchingWorkspaceRepository,
  readableWorkspaceRef,
  updateWorkspaceRepository,
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
});

test("the Task editor control offers admitted repositories and preserves a legacy selector", () => {
  const html = renderToStaticMarkup(WorkspaceRepositoryControls({
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
