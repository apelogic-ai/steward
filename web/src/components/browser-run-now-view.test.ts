import { describe, expect, test } from "bun:test";

import { runNowFailureMessage } from "./browser-run-now-view";

describe("Run now failures", () => {
  test("shows a bounded server message before a generic fallback", () => {
    expect(runNowFailureMessage({
      error: "task.persistence_failed",
      message: "Steward could not record this run.",
    })).toBe("Steward could not record this run.");
    expect(runNowFailureMessage({ error: "task.persistence_failed" }))
      .toBe("Run request failed (task.persistence_failed).");
    expect(runNowFailureMessage(undefined))
      .toBe("The run request was rejected. Check the package locator, inputs, and Envelope authority.");
  });
});
