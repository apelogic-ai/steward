import { expect, test } from "bun:test";

import { sampleRunDone } from "./onboarding-view";

test("onboarding completion requires the sample workflow and provisioned envelope", () => {
  const workflow = "repo-summary@1";
  const envelopes = new Set(["sample-envelope"]);
  expect(sampleRunDone([], workflow, envelopes)).toBe(false);
  expect(sampleRunDone([{ workflowName: "repo-summary", workflowVersion: 1 }], workflow, envelopes)).toBe(false);
  expect(sampleRunDone([{ trigger: { provider: "github" }, workflowName: "repository-review", workflowVersion: 1, userEnvelopeInstanceId: "sample-envelope" }], workflow, envelopes)).toBe(false);
  expect(sampleRunDone([{ trigger: { provider: "github" }, workflowName: "repo-summary", workflowVersion: 1, userEnvelopeInstanceId: "other-envelope" }], workflow, envelopes)).toBe(false);
  expect(sampleRunDone([{ trigger: { provider: "github" }, workflowName: "repo-summary", workflowVersion: 1, userEnvelopeInstanceId: "sample-envelope" }], workflow, envelopes)).toBe(true);
});
