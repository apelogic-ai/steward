import { describe, expect, test } from "bun:test";

import {
  inlineFiles,
  packageLocatorForTask,
  runNowFailureMessage,
} from "./browser-run-now-view";

const starterTask = {
  taskDefinition: {
    schemaVersion: "steward.task-definition/v2" as const,
    name: "hello-world",
    version: 2,
    runtime: { agentRef: "codex@0.140.0" },
    promptText: "Write hello to out/hello.txt.",
    skills: [],
    outputs: [{ path: "out/hello.txt", kind: "file" as const, required: true }],
  },
  inputs: { greeting: "hello" },
  executionLog: "full" as const,
  packagePath: ".steward/tasks/hello-world/task-definition.json",
};

test("inline Run now packages keep the prompt in the single TaskDefinition file", () => {
  const files = inlineFiles(
    starterTask,
    "codex@0.140.0",
    { provider: "openai", model: "gpt-5.4" },
  );

  expect(Object.keys(files)).toEqual([starterTask.packagePath]);
  expect(JSON.parse(files[starterTask.packagePath] ?? "{}")).toMatchObject({
    name: "hello-world",
    version: 2,
    promptText: "Write hello to out/hello.txt.",
    outputs: [{ path: "out/hello.txt", kind: "file", required: true }],
    runtime: { agentRef: "codex@0.140.0", model: { provider: "openai", model: "gpt-5.4" } },
  });
});

test("inline Run now authors the selected repository name and named ref", () => {
  const files = inlineFiles(
    starterTask,
    "codex@0.140.0",
    { provider: "openai", model: "gpt-5.4" },
    {
      repository: {
        name: "repository",
        owner: "example-org",
      },
      ref: "release-v1",
      historyDepth: 25,
    },
  );

  expect(JSON.parse(files[starterTask.packagePath] ?? "{}").workspace).toEqual([{
    git: {
      repository: "example-org/repository",
      ref: "git:ref:release-v1",
      history: { depth: 25 },
    },
  }]);
});

test("inline Run now keeps declared authority but follows a compatible model fallback", () => {
  const configured = {
    ...starterTask,
    taskDefinition: {
      ...starterTask.taskDefinition,
      runtime: {
        agentRef: "claude-code@2.1.222",
        model: { provider: "anthropic", model: "claude-sonnet-4-5" },
      },
      requires: {
        authority: {
          llms: [{ provider: "anthropic", model: "claude-sonnet-4-5" }],
          tools: [],
          budget: { monthlyLimit: "10.00", singleRunLimit: "1.00", currency: "USD" },
          ttl: "1h",
          runner: { platforms: ["linux" as const] },
        },
      },
    },
  };
  const files = inlineFiles(
    configured,
    "codex@0.140.0",
    { provider: "openai", model: "gpt-5.4" },
  );
  const definition = JSON.parse(files[configured.packagePath] ?? "{}");

  expect(definition.runtime).toEqual({
    agentRef: "codex@0.140.0",
    model: { provider: "openai", model: "gpt-5.4" },
  });
  expect(definition.requires.authority).toMatchObject({
    llms: [{ provider: "openai", model: "gpt-5.4" }],
    budget: { monthlyLimit: "10.00", singleRunLimit: "1.00", currency: "USD" },
    ttl: "1h",
    runner: { platforms: ["linux"] },
  });
});

test("exact Tasks preserve inline bytes but use only immutable locators for Git and published sources", () => {
  expect(packageLocatorForTask({
    files: { "task-definition.json": "{}" },
    path: "task-definition.json",
    revision: `steward:sha256:${"a".repeat(64)}`,
    source: "inline",
  })).toEqual({
    files: { "task-definition.json": "{}" },
    path: "task-definition.json",
    revision: `steward:sha256:${"a".repeat(64)}`,
    source: "inline",
  });
  expect(packageLocatorForTask({
    files: {},
    path: "catalog/review/task-definition.json",
    revision: `git:sha1:${"b".repeat(40)}`,
    source: "https://github.com/example-org/agentic-ops.git",
  })).toEqual({
    path: "catalog/review/task-definition.json",
    revision: `git:sha1:${"b".repeat(40)}`,
    source: "https://github.com/example-org/agentic-ops.git",
  });
  expect(packageLocatorForTask({
    files: { "prompt.md": "Review the repository." },
    path: "task-definition.json",
    revision: "steward:version:3",
    source: "steward:registry/repository-review",
  })).toEqual({
    path: "task-definition.json",
    revision: "steward:version:3",
    source: "steward:registry/repository-review",
  });
});

describe("Run now failures", () => {
  test("uses a plain generic message without exposing server text or codes", () => {
    expect(runNowFailureMessage({
      error: "task.persistence_failed",
      message: "Steward could not record this run.",
    })).toBe("The run request was rejected. Check the package, inputs, and selected Envelope limits, then try again.");
    expect(runNowFailureMessage({ error: "task.persistence_failed" }))
      .toBe("The run request was rejected. Check the package, inputs, and selected Envelope limits, then try again.");
    expect(runNowFailureMessage(undefined))
      .toBe("The run request was rejected. Check the package, inputs, and selected Envelope limits, then try again.");
  });

  test("replaces a known server reason with an actionable next step", () => {
    expect(runNowFailureMessage({
      error: "task.direct_package_source_disabled",
      failureReason: "Direct package source resolution is disabled.",
    })).toBe("The run request was rejected. Choose an inline task or ask an administrator to enable GitHub source packages.");
  });

  test("shows a human-readable rejection returned in the error field", () => {
    expect(runNowFailureMessage({
      error: "The selected coding agent does not support this model.",
    })).toBe("The run request was rejected. The selected coding agent does not support this model.");
  });

  test("shows a repository rejection reason and names the selected repository", () => {
    const message = runNowFailureMessage({
      error: "task.workspace_rejected",
      failureReason: "The requested workspace repository was not approved.",
    }, "example-org/repository");

    expect(message).toContain("The requested workspace repository was not approved.");
    expect(message).toContain("example-org/repository");
    expect(message).not.toContain("task.workspace_rejected");
  });

  test("shows a non-repository workspace rejection without blaming the selected repository", () => {
    const message = runNowFailureMessage({
      error: "task.workspace_rejected",
      failureReason: "Workspace history depth 25 exceeds the effective ceiling 10.",
    }, "example-org/repository");

    expect(message).toBe("The run request was rejected. Workspace history depth 25 exceeds the effective ceiling 10.");
    expect(message).not.toContain("example-org/repository");
  });
});
