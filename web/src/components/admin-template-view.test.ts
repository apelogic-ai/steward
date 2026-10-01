import { describe, expect, test } from "bun:test";

import { initialEnvelopeTemplate, templateMemberRoles, validateTemplateFields } from "./admin-template-view";

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
      memberRoles: [],
      models: [],
      monthlyLimit: "",
      singleRunLimit: "not-a-decimal",
      templateId: "not valid",
      thresholdJson: "not-json",
      tools: [{ provider: "github", resource: "repository", action: "write" }],
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
      memberRoles: ["engineering:member"],
      models: [{ provider: "provider-a", model: "model-a" }],
      monthlyLimit: "01.1234567",
      singleRunLimit: "0.",
      templateId: "engineering.v1",
      thresholdJson: "",
      tools: [],
    })).toEqual({});
  });
});
