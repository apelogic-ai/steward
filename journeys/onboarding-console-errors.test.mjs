import { describe, expect, test } from "bun:test";

import {
  onlyOnboardingRequestUnavailable,
  onlyRunWorkflowUnavailable,
  onlyZeroBudgetTaskSaveRejected,
} from "./onboarding-console-errors.mjs";

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

describe("onlyRunWorkflowUnavailable", () => {
  const rejection = {
    method: "POST",
    status: 503,
    url: "https://localhost:28443/app/api/v1/runs/task-1/github/workflow",
  };

  test("accepts exactly one 503 from the run workflow endpoint", () => {
    expect(onlyRunWorkflowUnavailable([
      {
        text: "Failed to load resource: the server responded with a status of 503",
        url: rejection.url,
      },
    ], [rejection], pageUrl)).toBe(true);
  });

  test.each([
    ["no errors", [], [rejection]],
    ["two matching errors", [
      { text: "Failed to load resource: 503", url: rejection.url },
      { text: "Failed to load resource: 503", url: rejection.url },
    ], [rejection]],
    ["another status", [{ text: "Failed to load resource: 500", url: rejection.url }], [rejection]],
    ["another endpoint", [{ text: "Failed to load resource: 503", url: "https://localhost:28443/app/api/v1/runs/task-1/github/evidence" }], [rejection]],
    ["a query string", [{ text: "Failed to load resource: 503", url: `${rejection.url}?retry=1` }], [{ ...rejection, url: `${rejection.url}?retry=1` }]],
    ["another origin", [{ text: "Failed to load resource: 503", url: "https://example.com/app/api/v1/runs/task-1/github/workflow" }], [{ ...rejection, url: "https://example.com/app/api/v1/runs/task-1/github/workflow" }]],
    ["a page console error", [{ text: "503 is not a valid state", url: "https://localhost:28443/_next/static/app.js" }], [rejection]],
    ["a missing source", [{ text: "Failed to load resource: 503", url: "" }], [rejection]],
    ["no matching response", [{ text: "Failed to load resource: 503", url: rejection.url }], []],
    ["two matching responses", [{ text: "Failed to load resource: 503", url: rejection.url }], [rejection, rejection]],
    ["another method", [{ text: "Failed to load resource: 503", url: rejection.url }], [{ ...rejection, method: "GET" }]],
    ["another response status", [{ text: "Failed to load resource: 503", url: rejection.url }], [{ ...rejection, status: 500 }]],
    ["another response endpoint", [{ text: "Failed to load resource: 503", url: rejection.url }], [{ ...rejection, url: "https://localhost:28443/app/api/v1/runs/task-1/github/evidence" }]],
    ["a response query string", [{ text: "Failed to load resource: 503", url: rejection.url }], [{ ...rejection, url: `${rejection.url}?retry=1` }]],
    ["another response origin", [{ text: "Failed to load resource: 503", url: rejection.url }], [{ ...rejection, url: "https://example.com/app/api/v1/runs/task-1/github/workflow" }]],
  ])("rejects %s", (_name, errors, responses) => {
    expect(onlyRunWorkflowUnavailable(errors, responses, pageUrl)).toBe(false);
  });
});

describe("onlyZeroBudgetTaskSaveRejected", () => {
  const rejection = {
    method: "POST",
    status: 422,
    url: "https://localhost:28443/app/api/v1/tasks",
  };

  test("accepts exactly one 422 from the Task save endpoint", () => {
    expect(onlyZeroBudgetTaskSaveRejected([
      {
        text: "Failed to load resource: the server responded with a status of 422",
        url: "https://localhost:28443/app/api/v1/tasks",
      },
    ], [rejection], pageUrl)).toBe(true);
  });

  test.each([
    ["no errors", []],
    ["two matching errors", [
      { text: "Failed to load resource: 422", url: "https://localhost:28443/app/api/v1/tasks" },
      { text: "Failed to load resource: 422", url: "https://localhost:28443/app/api/v1/tasks" },
    ]],
    ["another status", [{ text: "Failed to load resource: 500", url: "https://localhost:28443/app/api/v1/tasks" }]],
    ["another endpoint", [{ text: "Failed to load resource: 422", url: "https://localhost:28443/app/api/v1/session" }]],
    ["a query string", [{ text: "Failed to load resource: 422", url: "https://localhost:28443/app/api/v1/tasks?retry=1" }]],
    ["another origin", [{ text: "Failed to load resource: 422", url: "https://example.com/app/api/v1/tasks" }]],
    ["a page console error", [{ text: "422 is not a valid state", url: "https://localhost:28443/_next/static/app.js" }]],
    ["a missing source", [{ text: "Failed to load resource: 422", url: "" }]],
  ])("rejects %s", (_name, errors) => {
    expect(onlyZeroBudgetTaskSaveRejected(errors, [rejection], pageUrl)).toBe(false);
  });

  test.each([
    ["no matching response", []],
    ["two matching responses", [rejection, rejection]],
    ["another method", [{ ...rejection, method: "GET" }]],
    ["another status", [{ ...rejection, status: 400 }]],
    ["another endpoint", [{ ...rejection, url: "https://localhost:28443/app/api/v1/session" }]],
    ["another origin", [{ ...rejection, url: "https://example.com/app/api/v1/tasks" }]],
  ])("rejects %s for the HTTP response", (_name, responses) => {
    expect(onlyZeroBudgetTaskSaveRejected([{
      text: "Failed to load resource: 422",
      url: "https://localhost:28443/app/api/v1/tasks",
    }], responses, pageUrl)).toBe(false);
  });
});
