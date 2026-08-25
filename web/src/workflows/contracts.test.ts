import { describe, expect, test } from "bun:test";

import { workflowReference, workflowRenderRequest, type PublishedWorkflow } from "./contracts";

const repositoryReview: PublishedWorkflow = {
  agent: "codex@0.117.0",
  displayName: "Repository review",
  name: "repository-review",
  version: 1,
};

describe("versioned Workflow selection", () => {
  test("uses the exact name@version wire reference", () => {
    expect(workflowReference(repositoryReview)).toBe("repository-review@1");
  });

  test("the generator request contains no caller-selected execution authority", () => {
    expect(workflowRenderRequest(repositoryReview)).toEqual({
      workflow: "repository-review@1",
    });
    expect(workflowRenderRequest(repositoryReview)).not.toHaveProperty("codingAgentRuntime");
    expect(workflowRenderRequest(repositoryReview)).not.toHaveProperty("revision");
    expect(workflowRenderRequest(repositoryReview)).not.toHaveProperty("path");
  });
});
