import assert from "node:assert/strict";
import test from "node:test";

import { isIssue414RunFailure } from "./run-failure-markers.mjs";

const issue414 = {
  knownIssue: "@known-failing-414",
  statedSingleRunLimit: "0",
  phase: "failed",
  finalized: true,
  failureReason: null,
  atDeadline: false,
};

test("matches only the zero per-run budget failure recorded by issue 414", () => {
  assert.equal(isIssue414RunFailure(issue414), true);
  assert.equal(isIssue414RunFailure({ ...issue414, finalized: false, phase: "running", atDeadline: true }), true);

  for (const changed of [
    { knownIssue: "@known-failing-472" },
    { statedSingleRunLimit: "0.01" },
    { phase: "succeeded" },
    { failureReason: "runtime unavailable" },
    { finalized: false, phase: "queued", atDeadline: true },
    { finalized: false, phase: "running", atDeadline: false },
  ]) {
    assert.equal(isIssue414RunFailure({ ...issue414, ...changed }), false);
  }
});
