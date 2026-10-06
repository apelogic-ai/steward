import { expect, test } from "bun:test";

import { taskRunHref } from "./task-view";

test("Task runs preserve the package source contract", () => {
  expect(taskRunHref({
    contentDigest: "steward:sha256:example",
    name: "browser-task",
    source: "inline",
    version: 1,
  })).toBe("/runs/new?task=steward%3Asha256%3Aexample");
  expect(taskRunHref({
    contentDigest: "sha256:example",
    name: "repository-review",
    source: "steward:registry/repository-review",
    version: 3,
  })).toBe("/runs/new?task=sha256%3Aexample");
});
