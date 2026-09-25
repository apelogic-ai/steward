import { expect, test } from "bun:test";

import { workflowSetupDone } from "./onboarding-view";

test("workflow setup can be acknowledged before the first detected run", () => {
  const workflow = "repo-summary@1";
  expect(workflowSetupDone([], false, workflow)).toBe(false);
  expect(workflowSetupDone([], true, workflow)).toBe(true);
  expect(workflowSetupDone([{ workflowName: "repository-review", workflowVersion: 1 }], false, workflow)).toBe(false);
  expect(workflowSetupDone([{ workflowName: "repo-summary", workflowVersion: 1 }], false, workflow)).toBe(true);
  expect(workflowSetupDone([{ workflowName: "repo-summary", workflowVersion: 2 }], false, workflow)).toBe(false);
});
