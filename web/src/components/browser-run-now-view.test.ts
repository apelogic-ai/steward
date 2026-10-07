import { describe, expect, test } from "bun:test";

import {
  compatibleAgents,
  effectiveAgentSelection,
  effectiveModelSelection,
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
  test("shows a bounded server message before a generic fallback", () => {
    expect(runNowFailureMessage({
      error: "task.persistence_failed",
      message: "Steward could not record this run.",
    })).toBe("task.persistence_failed: Steward could not record this run.");
    expect(runNowFailureMessage({ error: "task.persistence_failed" }))
      .toBe("Run request failed (task.persistence_failed).");
    expect(runNowFailureMessage(undefined))
      .toBe("The run request was rejected. Check the package locator, inputs, and Envelope authority.");
  });

  test("shows the API code, failure reason, and an actionable known-code hint", () => {
    expect(runNowFailureMessage({
      error: "task.direct_package_source_disabled",
      failureReason: "Direct package source resolution is disabled.",
    })).toBe("task.direct_package_source_disabled: Direct package source resolution is disabled. Choose an inline task or ask an administrator to enable GitHub source packages.");
  });
});

describe("inline agent compatibility", () => {
  test("only preselects agents whose model family is allowed by the selected envelope", () => {
    const agents = compatibleAgents(["codex@0.140.0", "claude-code@2.1.222"], [
      { provider: "openai", model: "gpt-5.4" },
    ]);
    expect(agents).toEqual([
      { agentRef: "codex@0.140.0", model: { provider: "openai", model: "gpt-5.4" }, compatible: true, reason: null },
      { agentRef: "claude-code@2.1.222", model: null, compatible: false, reason: "Requires an Anthropic model, which this Envelope does not allow." },
    ]);
    expect(effectiveAgentSelection(agents, "claude-code@2.1.222")).toEqual({
      selected: agents[0],
      warning: "claude-code@2.1.222 is not allowed by this Envelope. Using codex@0.140.0 instead.",
    });
  });

  test("uses the configured model when allowed and warns on a compatible model fallback", () => {
    const selectedAgent = {
      agentRef: "codex@0.140.0",
      model: { provider: "openai", model: "gpt-5.4" },
      compatible: true,
      reason: null,
    };
    const configured = { provider: "openai", model: "gpt-5.5" };
    expect(effectiveModelSelection(configured, selectedAgent.agentRef, selectedAgent, [
      selectedAgent.model,
      configured,
    ])).toEqual({ selected: configured, warning: null });
    expect(effectiveModelSelection(configured, selectedAgent.agentRef, selectedAgent, [
      selectedAgent.model,
    ])).toEqual({
      selected: selectedAgent.model,
      warning: "openai/gpt-5.5 is not allowed by this Envelope. Using openai/gpt-5.4 instead.",
    });
  });
});
