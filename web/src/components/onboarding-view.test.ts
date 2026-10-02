import { expect, test } from "bun:test";

import type { BrowserRunView } from "@/api-client";
import { automatedPackageRun } from "@/data/onboarding-progress";

import { browserHelloWorldRun } from "./onboarding-view";

function run(overrides: Partial<BrowserRunView>): BrowserRunView {
  return {
    codingAgentRuntime: "codex@0.140.0",
    createdAt: "2026-10-02T00:00:00Z",
    finalizationRequested: false,
    finalized: true,
    origin: "browser",
    phase: "succeeded",
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
    package: { source: "https://github.com/example-org/example-repo.git", revision: "git:sha1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", path: "task-definition.json" },
    userEnvelopeInstanceId: "sample-envelope",
  })], envelopes)).toBeUndefined();
  expect(browserHelloWorldRun([run({
    origin: "browser",
    package: { source: "inline", revision: "steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", path: "task-definition.json" },
    userEnvelopeInstanceId: "other-envelope",
  })], envelopes)).toBeUndefined();
  expect(browserHelloWorldRun([run({
    origin: "browser",
    package: { source: "inline", revision: "steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", path: "task-definition.json" },
    userEnvelopeInstanceId: "sample-envelope",
  })], envelopes)?.package?.source).toBe("inline");
});

test("optional automation completes only for the exact successful browser package", () => {
  const envelopeIds = new Set(["sample-envelope"]);
  const browser = run({
    package: { source: "inline", revision: `steward:sha256:${"a".repeat(64)}`, path: "task-definition.json", contentDigest: `steward:sha256:${"a".repeat(64)}` },
    userEnvelopeInstanceId: "sample-envelope",
  });
  const unrelated = run({
    origin: "github-actions",
    package: { source: "https://github.com/example-org/agentic-ops.git", revision: `git:sha1:${"b".repeat(40)}`, path: "task-definition.json", contentDigest: `steward:sha256:${"b".repeat(64)}` },
    trigger: { provider: "github", repository: "example-org/agentic-ops", event: "workflow_dispatch", actor: "alice", ref: "refs/heads/main", sha: "b".repeat(40), runId: "1", runAttempt: 1, runUrl: "https://github.com/example-org/agentic-ops/actions/runs/1", callerWorkflow: ".github/workflows/steward.yml" },
    userEnvelopeInstanceId: "sample-envelope",
  });
  expect(automatedPackageRun([unrelated], browser, envelopeIds)).toBeUndefined();
  expect(automatedPackageRun([{ ...unrelated, package: { ...unrelated.package!, contentDigest: browser.package!.contentDigest } }], browser, envelopeIds)?.trigger?.provider).toBe("github");
});
