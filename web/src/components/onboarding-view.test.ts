import { expect, test } from "bun:test";

import { workflowSetupDone } from "./onboarding-view";

test("workflow setup can be acknowledged before the first detected run", () => {
  const workflow = "dependency-audit@1";
  expect(workflowSetupDone([], false, workflow)).toBe(false);
  expect(workflowSetupDone([], true, workflow)).toBe(true);
  expect(workflowSetupDone([{ workflowName: "dependency-audit", workflowVersion: 1 }], false, workflow)).toBe(false);
  expect(workflowSetupDone([{ trigger: { provider: "github" }, workflowName: "repository-review", workflowVersion: 1 }], false, workflow)).toBe(false);
  expect(workflowSetupDone([{ trigger: { provider: "github" }, workflowName: "dependency-audit", workflowVersion: 1 }], false, workflow)).toBe(true);
  expect(workflowSetupDone([{ trigger: { provider: "github" }, workflowName: "dependency-audit", workflowVersion: 2 }], false, workflow)).toBe(false);
});
