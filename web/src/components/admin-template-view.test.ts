import { describe, expect, test } from "bun:test";

import { initialEnvelopeTemplate, templateMemberRoles, validateTemplateFields } from "./admin-template-view";

function authoredEnvelope() {
  return {
    revision: 1,
    spec: {
      budget: { currency: "USD", monthlyLimit: "10.00", singleRunLimit: "1.00" },
      llms: [{ provider: "provider-a", model: "model-a" }],
      tools: [],
      runtimeMinutesLimit: "60",
      ttl: "15m",
      runner: { platforms: ["linux" as const], memory: "2Gi", compute: "1", storage: "10Gi" },
    },
  };
}

describe("first envelope template", () => {
  test("starts as an editable, least-authority version-one template", () => {
    expect(initialEnvelopeTemplate).toEqual({
      revision: 1,
      spec: {
        budget: { currency: "USD", monthlyLimit: "0.10", singleRunLimit: "0.10" },
        llms: [],
        tools: [],
        ttl: "15m",
        runner: { platforms: ["linux"] },
        runtimeMinutesLimit: "60",
      },
    });
  });

  test("follows the typed template ID until member roles are explicitly edited", () => {
    expect(templateMemberRoles("engineer", [], false)).toEqual(["engineer"]);
    expect(templateMemberRoles(" analyst ", [], false)).toEqual(["analyst"]);
    expect(templateMemberRoles("operator", ["analyst", "reviewer"], true)).toEqual(["analyst", "reviewer"]);
  });

  test("reports each invalid authored field without reducing it to one rejection", () => {
    const errors = validateTemplateFields({
      allowedModels: new Set([JSON.stringify(["provider-a", "model-a"])]),
      allowedTools: new Set([JSON.stringify(["github", "repository", "read"])]),
      autoApproveToCeiling: false,
      displayName: "",
      envelope: authoredEnvelope(),
      memberRoles: [],
      models: [],
      monthlyLimit: "",
      singleRunLimit: "not-a-decimal",
      templateId: "not valid",
      thresholdJson: "not-json",
      tools: [{ provider: "github", resource: "repository", action: "write" }],
      workspaceJson: "",
    });

    expect(errors).toEqual({
      templateId: "Use 1–128 letters, numbers, periods, underscores, hyphens, or colons; start with a letter or number.",
      displayName: "Enter a display name.",
      memberRoles: "Add at least one eligible member role.",
      monthlyLimit: "Enter a monthly budget.",
      singleRunLimit: "Enter a non-negative decimal.",
      models: "Select at least one model.",
      tools: "Remove or replace every tool not listed in the capability catalog.",
      threshold: "Enter a complete valid envelope as JSON.",
    });
  });

  test("accepts the server-compatible identifier and decimal grammar", () => {
    expect(validateTemplateFields({
      allowedModels: new Set([JSON.stringify(["provider-a", "model-a"])]),
      allowedTools: new Set(),
      autoApproveToCeiling: true,
      displayName: "Engineering",
      envelope: authoredEnvelope(),
      memberRoles: ["engineering:member"],
      models: [{ provider: "provider-a", model: "model-a" }],
      monthlyLimit: "01.1234567",
      singleRunLimit: "0.",
      templateId: "engineering.v1",
      thresholdJson: "",
      tools: [],
      workspaceJson: "",
    })).toEqual({});
  });

  test("validates the optional workspace authority before submission", () => {
    const common = {
      allowedModels: new Set([JSON.stringify(["provider-a", "model-a"])]),
      allowedTools: new Set<string>(),
      autoApproveToCeiling: true,
      displayName: "Engineering",
      envelope: authoredEnvelope(),
      memberRoles: ["engineer"],
      models: [{ provider: "provider-a", model: "model-a" }],
      monthlyLimit: "10.00",
      singleRunLimit: "1.00",
      templateId: "engineer",
      thresholdJson: "",
      tools: [],
    };
    expect(validateTemplateFields({
      ...common,
      workspaceJson: JSON.stringify({
        allowedTypes: ["git", "scratch"],
        maxTotalSize: "2Gi",
        maxFiles: 100_000,
        maxHistoryDepth: 1_000,
        maxSubmoduleDepth: 4,
      }),
    })).toEqual({});
    expect(validateTemplateFields({
      ...common,
      workspaceJson: JSON.stringify({ allowedTypes: ["git"], maxTotalSize: "2GB" }),
    })).toEqual({
      workspace: "Enter a complete valid workspace authority object or leave it blank.",
    });
  });

  test("rejects valid JSON when the threshold is invalid or wider than its ceiling", () => {
    const ceiling = authoredEnvelope();
    const threshold = authoredEnvelope();
    threshold.revision = 2;
    threshold.spec.budget.monthlyLimit = "11.00";
    threshold.spec.ttl = "not-a-duration";
    threshold.spec.runner.memory = "2GB";

    expect(validateTemplateFields({
      allowedModels: new Set([JSON.stringify(["provider-a", "model-a"])]),
      allowedTools: new Set(),
      autoApproveToCeiling: false,
      displayName: "Engineering",
      envelope: ceiling,
      memberRoles: ["engineer"],
      models: ceiling.spec.llms,
      monthlyLimit: "10.00",
      singleRunLimit: "1.00",
      templateId: "engineer",
      thresholdJson: JSON.stringify(threshold),
      tools: [],
      workspaceJson: "",
    })).toEqual({ threshold: "Enter a valid envelope within this template ceiling." });
  });

  test("reports invalid lifetime and runner quantities before submission", () => {
    const envelope = authoredEnvelope();
    envelope.spec.ttl = "";
    envelope.spec.runtimeMinutesLimit = "many";
    envelope.spec.runner.memory = "2GB";
    envelope.spec.runner.compute = "0";
    envelope.spec.runner.storage = "lots";

    expect(validateTemplateFields({
      allowedModels: new Set([JSON.stringify(["provider-a", "model-a"])]),
      allowedTools: new Set(),
      autoApproveToCeiling: true,
      displayName: "Engineering",
      envelope,
      memberRoles: ["engineer"],
      models: envelope.spec.llms,
      monthlyLimit: "10.00",
      singleRunLimit: "1.00",
      templateId: "engineer",
      thresholdJson: "",
      tools: [],
      workspaceJson: "",
    })).toEqual({
      ttl: "Enter a TTL.",
      runtimeMinutes: "Enter a non-negative decimal.",
      memory: "Use a positive binary quantity, such as 2Gi or 512Mi.",
      compute: "Use positive cores or millicores, such as 1 or 500m.",
      storage: "Use a positive binary quantity, such as 2Gi or 512Mi.",
    });
  });

  test("matches the server limits for display names and member roles", () => {
    const envelope = authoredEnvelope();
    expect(validateTemplateFields({
      allowedModels: new Set([JSON.stringify(["provider-a", "model-a"])]),
      allowedTools: new Set(),
      autoApproveToCeiling: true,
      displayName: "x".repeat(129),
      envelope,
      memberRoles: Array.from({ length: 65 }, (_, index) => `role-${index}`),
      models: envelope.spec.llms,
      monthlyLimit: "10.00",
      singleRunLimit: "1.00",
      templateId: "engineer",
      thresholdJson: "",
      tools: [],
      workspaceJson: "",
    })).toEqual({
      displayName: "Use at most 128 characters.",
      memberRoles: "Use at most 64 eligible member roles.",
    });
  });
});
