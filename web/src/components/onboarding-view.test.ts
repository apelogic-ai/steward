import { expect, test } from "bun:test";

import type { BrowserRunView, GithubRepositoryView } from "@/api-client";
import { automatedPackageRun } from "@/data/onboarding-progress";

import { browserHelloWorldRun, governedJobOnly, repositoryOptionLabel, repositoryReadinessMessage } from "./onboarding-view";

function run(overrides: Partial<BrowserRunView>): BrowserRunView {
  return {
    codingAgentRuntime: "codex@0.140.0",
    createdAt: "2026-10-02T00:00:00Z",
    executionLog: "off",
    finalizationRequested: false,
    finalized: true,
    origin: "browser",
    phase: "succeeded",
    rerunSupported: true,
    runtimeOwnership: "provisioned",
    stages: [],
    taskUid: "task-1",
    updatedAt: "2026-10-02T00:01:00Z",
    workflow: "browser-task@1",
    ...overrides,
  };
}

test("browser hello world progress accepts only an inline browser run under a provisioned envelope", () => {
  const envelopes = new Set(["sample-envelope"]);
  expect(browserHelloWorldRun([], envelopes)).toBeUndefined();
  expect(browserHelloWorldRun([run({
    origin: "browser",
    package: { source: "https://github.com/example-org/example-repo.git", revision: "git:sha1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", path: "task-definition.json", promptSource: "path" },
    userEnvelopeInstanceId: "sample-envelope",
  })], envelopes)).toBeUndefined();
  expect(browserHelloWorldRun([run({
    origin: "browser",
    package: { source: "inline", revision: "steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", path: "task-definition.json", promptSource: "inline" },
    userEnvelopeInstanceId: "other-envelope",
  })], envelopes)).toBeUndefined();
  expect(browserHelloWorldRun([run({
    origin: "browser",
    package: { source: "inline", revision: "steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", path: "task-definition.json", promptSource: "inline" },
    userEnvelopeInstanceId: "sample-envelope",
  })], envelopes)?.package?.source).toBe("inline");
});

test("browser hello world progress keeps the newest succeeded finalized run", () => {
  const envelopes = new Set(["sample-envelope"]);
  const succeeded = run({
    taskUid: "successful-task",
    updatedAt: "2026-10-02T00:01:00Z",
    package: { source: "inline", revision: `steward:sha256:${"a".repeat(64)}`, path: "task-definition.json", promptSource: "inline" },
    userEnvelopeInstanceId: "sample-envelope",
  });
  const laterFailure = run({
    taskUid: "failed-task",
    phase: "failed",
    updatedAt: "2026-10-02T00:02:00Z",
    package: { source: "inline", revision: `steward:sha256:${"b".repeat(64)}`, path: "task-definition.json", promptSource: "inline" },
    userEnvelopeInstanceId: "sample-envelope",
  });
  const laterRunning = run({
    taskUid: "running-task",
    finalized: false,
    phase: "running",
    updatedAt: "2026-10-02T00:03:00Z",
    package: { source: "inline", revision: `steward:sha256:${"c".repeat(64)}`, path: "task-definition.json", promptSource: "inline" },
    userEnvelopeInstanceId: "sample-envelope",
  });

  expect(browserHelloWorldRun([succeeded, laterFailure, laterRunning], envelopes)?.taskUid).toBe("successful-task");
});

test("GitHub automation completes only for the exact successful browser package", () => {
  const envelopeIds = new Set(["sample-envelope"]);
  const browser = run({
    package: { source: "inline", revision: `steward:sha256:${"a".repeat(64)}`, path: "task-definition.json", contentDigest: `steward:sha256:${"a".repeat(64)}`, promptSource: "inline" },
    userEnvelopeInstanceId: "sample-envelope",
  });
  const unrelated = run({
    origin: "github-actions",
    package: { source: "https://github.com/example-org/agentic-ops.git", revision: `git:sha1:${"b".repeat(40)}`, path: "task-definition.json", contentDigest: `steward:sha256:${"b".repeat(64)}`, promptSource: "path" },
    trigger: { provider: "github", repository: "example-org/agentic-ops", event: "workflow_dispatch", actor: "alice", ref: "refs/heads/main", sha: "b".repeat(40), runId: "1", runAttempt: 1, runUrl: "https://github.com/example-org/agentic-ops/actions/runs/1", callerWorkflow: ".github/workflows/steward.yml" },
    userEnvelopeInstanceId: "sample-envelope",
  });
  expect(automatedPackageRun([unrelated], browser, envelopeIds)).toBeUndefined();
  expect(automatedPackageRun([{ ...unrelated, package: { ...unrelated.package!, contentDigest: browser.package!.contentDigest } }], browser, envelopeIds)?.trigger?.provider).toBe("github");
});

test("governed job preview keeps the input preparation and reusable workflow jobs", () => {
  const workflow = [
    "jobs:",
    "  prepare:",
    "    runs-on: ubuntu-latest",
    "  governed:",
    "    needs: prepare",
    "    permissions:",
    "      id-token: write",
    "    uses: example-org/steward-run/.github/workflows/steward-task.yml@1111111111111111111111111111111111111111",
    "    with:",
    "      package-path: .steward/tasks/hello/task-definition.json",
    "  verify:",
    "    runs-on: ubuntu-latest",
    "",
  ].join("\n");
  const preview = governedJobOnly(workflow);
  expect(preview).toContain("jobs:\n  prepare:");
  expect(preview).toContain("\n  governed:");
  expect(preview).toContain("id-token: write");
  expect(preview).toContain("package-path: .steward/tasks/hello/task-definition.json");
  expect(preview).not.toContain("verify:");
});

function repository(overrides: Partial<GithubRepositoryView>): GithubRepositoryView {
  return {
    defaultBranch: "main",
    name: "agentic-ops",
    owner: "example-org",
    ownerId: "org_example",
    private: true,
    ready: true,
    repositoryId: "repo_agentic_ops",
    url: "https://github.com/example-org/agentic-ops",
    ...overrides,
  };
}

test("repository options name each repository and its admission state", () => {
  expect(repositoryOptionLabel(repository({}))).toBe("example-org/agentic-ops · Ready");
  expect(repositoryOptionLabel(repository({ name: "not-admitted", ready: false, missingPrerequisite: "source_repository_not_admitted" })))
    .toBe("example-org/not-admitted · Not admitted");
  expect(repositoryOptionLabel(repository({ name: "other", ready: false, missingPrerequisite: "workflow_scope_missing" })))
    .toBe("example-org/other · Not ready");
});

test("repository readiness names the selected repository or states that none is admitted", () => {
  const ready = repository({});
  const notAdmitted = repository({ name: "not-admitted", repositoryId: "repo_not_admitted", ready: false, missingPrerequisite: "source_repository_not_admitted" });
  const otherNotAdmitted = repository({ name: "legacy", repositoryId: "repo_legacy", ready: false, missingPrerequisite: "source_repository_not_admitted" });
  const missingScope = repository({ name: "scoped", repositoryId: "repo_scoped", ready: false, missingPrerequisite: "workflow_scope_missing" });

  expect(repositoryReadinessMessage([ready, notAdmitted], ready)).toBeNull();
  expect(repositoryReadinessMessage([ready, notAdmitted], undefined)).toBeNull();
  expect(repositoryReadinessMessage([ready, notAdmitted], notAdmitted))
    .toBe("example-org/not-admitted is not admitted as a governed source on this deployment; ask an administrator to add it.");
  expect(repositoryReadinessMessage([ready, missingScope], missingScope))
    .toBe("example-org/scoped is not ready for governed automation: workflow_scope_missing.");
  expect(repositoryReadinessMessage([notAdmitted, otherNotAdmitted], otherNotAdmitted))
    .toBe("None of your repositories is admitted as a governed source on this deployment; ask an administrator to add it.");
  expect(repositoryReadinessMessage([notAdmitted, missingScope], missingScope))
    .toBe("example-org/scoped is not ready for governed automation: workflow_scope_missing.");
});
