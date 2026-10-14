import { describe, expect, test } from "bun:test";

import { onlyOnboardingRequestUnavailable } from "./onboarding-console-errors.mjs";

const pageUrl = "https://localhost:28443/get-started";

describe("onlyOnboardingRequestUnavailable", () => {
  test("accepts only 503 errors from the run onboarding request family", () => {
    expect(onlyOnboardingRequestUnavailable([
      {
        text: "Failed to load resource: the server responded with a status of 503",
        url: "https://localhost:28443/app/api/v1/runs/task-1/github/onboarding",
      },
      {
        text: "Failed to load resource: the server responded with a status of 503",
        url: "https://localhost:28443/app/api/v1/runs/task-1/github/workflow",
      },
    ], pageUrl)).toBe(true);
  });

  test.each([
    ["no errors", []],
    ["another status", [{ text: "Failed to load resource: 500", url: "https://localhost:28443/app/api/v1/runs/task-1/github/onboarding" }]],
    ["another endpoint", [{ text: "Failed to load resource: 503", url: "https://localhost:28443/app/api/v1/session" }]],
    ["another origin", [{ text: "Failed to load resource: 503", url: "https://example.com/app/api/v1/runs/task-1/github/onboarding" }]],
    ["a page console error", [{ text: "503 is not a valid state", url: "https://localhost:28443/_next/static/app.js" }]],
    ["a missing source", [{ text: "Failed to load resource: 503", url: "" }]],
  ])("rejects %s", (_name, errors) => {
    expect(onlyOnboardingRequestUnavailable(errors, pageUrl)).toBe(false);
  });
});
