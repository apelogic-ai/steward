import { describe, expect, test } from "bun:test";

import { browserInputsRequired } from "./run-environment.mjs";

describe("journey environment validation", () => {
  test("caller-only selections do not require browser inputs", () => {
    for (const tags of [
      "@callers",
      "@callers and not @live-jira-webhook",
      "@callers and not @covered-by-process-e2e",
      "@callers and not @live-jira-webhook and not @covered-by-process-e2e",
    ]) {
      expect(browserInputsRequired(tags)).toBeFalse();
    }
  });

  test("other selections still require browser inputs", () => {
    expect(browserInputsRequired("@smoke")).toBeTrue();
    expect(browserInputsRequired("@callers or @smoke")).toBeTrue();
  });
});
