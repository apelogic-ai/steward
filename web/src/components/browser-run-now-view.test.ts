import { describe, expect, test } from "bun:test";

import { compatibleAgents, runNowFailureMessage } from "./browser-run-now-view";

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
  });
});
