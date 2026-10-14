import assert from "node:assert/strict";
import test from "node:test";

import { expectedKnownFailures } from "./known-failing-report.mjs";

function report(tags, marker) {
  return {
    errors: [],
    suites: [{
      specs: [{
        tags,
        tests: [{
          results: [{
            status: "failed",
            errors: [{ message: marker }],
          }],
        }],
      }],
    }],
  };
}

test("accepts Playwright JSON tags without the Gherkin at-sign", () => {
  assert.equal(expectedKnownFailures(
    report(["extended", "known-failing-414"], "[steward-known-failure:#414] expected failure"),
    1,
  ), true);
});

test("requires the marker issue to match the scenario tag", () => {
  assert.equal(expectedKnownFailures(
    report(["extended", "known-failing-414"], "[steward-known-failure:#464] different failure"),
    1,
  ), false);
});
