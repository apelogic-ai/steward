import { expect, test } from "bun:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { readFileSync } from "node:fs";

import type { GithubRepositoryView } from "@/api-client";

import {
  canonicalWorkspaceRef,
  defaultWorkspaceName,
  editWorkspaceReference,
  matchingWorkspaceRepository,
  readableWorkspaceRef,
  updateWorkspaceRepository,
  validWorkspaceBranchOrTag,
  WorkspaceRepositoryControls,
} from "./workspace-repository-controls";

const workspaceNameCases = JSON.parse(readFileSync(new URL("../../../tests/fixtures/workspace-name-cases.json", import.meta.url), "utf8")) as Array<{
  repositoryName: string;
  workspaceName: string | null;
}>;

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

test("Task-form workspace paths follow the server workspace-name cases", () => {
  for (const { repositoryName, workspaceName } of workspaceNameCases) {
    expect(defaultWorkspaceName(repositoryName)).toBe(workspaceName);
  }
});

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
  expect(readableWorkspaceRef("trigger")).toBe("trigger");
  expect(readableWorkspaceRef(`git:sha1:${"a".repeat(40)}`)).toBe(`git:sha1:${"a".repeat(40)}`);
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

test("the branch or tag field rejects selectors that are not valid named refs", () => {
  expect(validWorkspaceBranchOrTag("main")).toBeTrue();
  expect(validWorkspaceBranchOrTag("git:ref:main")).toBeTrue();
  expect(validWorkspaceBranchOrTag("trigger")).toBeFalse();
  expect(validWorkspaceBranchOrTag("HEAD")).toBeFalse();
  expect(validWorkspaceBranchOrTag("deadbee")).toBeFalse();
  expect(validWorkspaceBranchOrTag("feature..branch")).toBeFalse();
  expect(validWorkspaceBranchOrTag("feature\u0085branch")).toBeFalse();
  expect(validWorkspaceBranchOrTag("a".repeat(512))).toBeTrue();
  expect(validWorkspaceBranchOrTag("a".repeat(513))).toBeFalse();
  expect(validWorkspaceBranchOrTag("é".repeat(256))).toBeTrue();
  expect(validWorkspaceBranchOrTag("é".repeat(257))).toBeFalse();
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

test("the Task editor shows the trigger selector instead of the repository default", () => {
  const html = renderToStaticMarkup(createElement(WorkspaceRepositoryControls, {
    onChange: () => undefined,
    repositories: [repository],
    value: JSON.stringify([{
      git: {
        repository: "example-org/repository",
        ref: "trigger",
      },
    }]),
  }));

  expect(html).toContain('value="trigger"');
  expect(html).not.toContain('value="main"');
  expect(html).toContain("Workspace entry example-org/repository accepts trigger");
});

test("the Task editor shows each repository checkout path and access", () => {
  const html = renderToStaticMarkup(createElement(WorkspaceRepositoryControls, {
    onChange: () => undefined,
    repositories: [repository],
    value: JSON.stringify([
      {
        git: {
          repository: "example-org/repository",
          ref: "git:ref:main",
        },
      },
      {
        git: {
          access: "copy",
          name: "source",
          repository: "example-org/other",
          ref: "git:ref:main",
        },
      },
    ]),
  }));

  expect(html).toContain('<span>example-org/repository</span><code class="text-ink">/sandbox/workspace/repository</code><span>· read-only</span>');
  expect(html).toContain('<span>example-org/other</span><code class="text-ink">/sandbox/workspace/source</code><span>· copy</span>');
});

test("the Task editor uses catalog names for stable IDs and server-equivalent path normalization", () => {
  const siteRepository = { ...repository, name: "site.git", repositoryId: "1002" };
  const html = renderToStaticMarkup(createElement(WorkspaceRepositoryControls, {
    onChange: () => undefined,
    repositories: [siteRepository],
    value: JSON.stringify([{
      git: {
        repository: { ownerId: "1000", repositoryId: "1002" },
        ref: "git:ref:main",
      },
    }]),
  }));

  expect(html).toContain('<span>example-org/site.git</span><code class="text-ink">/sandbox/workspace/site-git</code><span>· read-only</span>');
});

test("the Task editor keeps unresolved repository entries visible without inventing a checkout path", () => {
  const html = renderToStaticMarkup(createElement(WorkspaceRepositoryControls, {
    onChange: () => undefined,
    repositories: [],
    value: JSON.stringify([{ git: { repository: "self", ref: "trigger" } }]),
  }));

  expect(html).toContain("Current repository");
  expect(html).toContain("Checkout path resolves when the run starts");
  expect(html).toContain("· read-only");
  expect(html).not.toContain("/sandbox/workspace/self");
});
