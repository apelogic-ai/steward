import { expect, test } from "bun:test";

import { workflowSetupDone } from "./onboarding-view";

test("workflow setup can be acknowledged before the first detected run", () => {
  const path = ".github/workflows/steward-repo-summary.yml";
  expect(workflowSetupDone([], false, path)).toBe(false);
  expect(workflowSetupDone([], true, path)).toBe(true);
  expect(workflowSetupDone([{ trigger: { callerWorkflow: "example-org/repo/.github/workflows/other.yml@refs/heads/main" } }], false, path)).toBe(false);
  expect(workflowSetupDone([{ trigger: { callerWorkflow: "example-org/repo/.github/workflows/steward-repo-summary.yml@refs/heads/main" } }], false, path)).toBe(true);
});
