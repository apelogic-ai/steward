import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import { initialEnvelopeTemplate, templateBudgetCurrency, templateMemberRoles, validateTemplateFields } from "./admin-template-view";

const source = readFileSync(new URL("./admin-template-view.tsx", import.meta.url), "utf8");

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
        budget: { currency: "USD", monthlyLimit: "", singleRunLimit: "" },
        llms: [],
        tools: [],
        ttl: "",
        runner: { platforms: [] },
        runtimeMinutesLimit: "",
      },
    });
    expect(source).not.toContain("capabilities.models.slice(0, 1)");
    expect(source).toContain("allowInlineBrowserTasks={false}");
  });

  test("preserves the template currency and explains TTL duration units", () => {
    const envelope = authoredEnvelope();
    envelope.spec.budget.currency = "EUR";

    expect(templateBudgetCurrency(envelope)).toBe("EUR");
    expect(source).toContain("currency: budgetCurrency");
    expect(source).not.toContain("Limit per run (USD)");
    expect(source).not.toContain("Monthly limit (USD)");
    expect(source).toContain('unitAriaLabel="Time-to-live unit"');
    expect(source).toContain('mode="minutes"');
  });

  test("does not turn the template ID into an eligible member role", () => {
    expect(templateMemberRoles([])).toEqual([]);
    expect(templateMemberRoles(["analyst", "reviewer"])).toEqual(["analyst", "reviewer"]);
  });

  test("reports each invalid authored field without reducing it to one rejection", () => {
    const errors = validateTemplateFields({
      allowedModels: new Set([JSON.stringify(["provider-a", "model-a"])]),
      allowedTools: new Set([JSON.stringify(["github", "repository", "read"])]),
      displayName: "",
      envelope: authoredEnvelope(),
      memberRoles: [],
      models: [],
      monthlyLimit: "",
      singleRunLimit: "not-a-decimal",
      templateId: "not valid",
      tools: [{ provider: "github", resource: "repository", action: "write" }],
      workspace: undefined,
    });

    expect(errors).toEqual({
      templateId: "Use 1–128 letters, numbers, periods, underscores, hyphens, or colons; start with a letter or number.",
      displayName: "Enter a display name.",
      memberRoles: "Add at least one eligible member role.",
      monthlyLimit: "Enter a monthly limit.",
      singleRunLimit: "Enter a non-negative decimal.",
      models: "Select at least one model.",
      tools: "Remove or replace every tool that is not available in this installation.",
    });
  });

  test("accepts the server-compatible identifier and decimal grammar", () => {
    expect(validateTemplateFields({
      allowedModels: new Set([JSON.stringify(["provider-a", "model-a"])]),
      allowedTools: new Set(),
      displayName: "Engineering",
      envelope: authoredEnvelope(),
      memberRoles: ["engineering:member"],
      models: [{ provider: "provider-a", model: "model-a" }],
      monthlyLimit: "01.1234567",
      singleRunLimit: "0.",
      templateId: "engineering.v1",
      tools: [],
      workspace: undefined,
    })).toEqual({});
  });

  test("requires a server-compatible budget currency before submission", () => {
    const envelope = authoredEnvelope();
    envelope.spec.budget.currency = "usd";

    expect(validateTemplateFields({
      allowedModels: new Set([JSON.stringify(["provider-a", "model-a"])]),
      allowedTools: new Set(),
      displayName: "Engineering",
      envelope,
      memberRoles: ["engineer"],
      models: envelope.spec.llms,
      monthlyLimit: "10.00",
      singleRunLimit: "1.00",
      templateId: "engineer",
      tools: [],
      workspace: undefined,
    })).toEqual({
      currency: "Use a three-letter uppercase currency code.",
    });
  });

  test("validates the optional workspace authority before submission", () => {
    const common = {
      allowedModels: new Set([JSON.stringify(["provider-a", "model-a"])]),
      allowedTools: new Set<string>(),
      displayName: "Engineering",
      envelope: authoredEnvelope(),
      memberRoles: ["engineer"],
      models: [{ provider: "provider-a", model: "model-a" }],
      monthlyLimit: "10.00",
      singleRunLimit: "1.00",
      templateId: "engineer",
      tools: [],
    };
    expect(validateTemplateFields({
      ...common,
      workspace: { allowedTypes: ["git"] },
    })).toEqual({});
    expect(validateTemplateFields({
      ...common,
      workspace: { allowedTypes: ["git"], maxTotalSize: "2Gi" },
    })).toEqual({});
    expect(validateTemplateFields({
      ...common,
      workspace: { allowedTypes: ["git"], maxTotalSize: "1.5Gi" },
    })).toEqual({});
    expect(validateTemplateFields({
      ...common,
      workspace: {
        allowedTypes: ["git", "scratch"],
        maxTotalSize: "2Gi",
        maxFiles: 100_000,
        maxHistoryDepth: 1_000,
        maxSubmoduleDepth: 4,
      },
    })).toEqual({});
    expect(validateTemplateFields({
      ...common,
      workspace: { allowedTypes: [], maxTotalSize: "2Gi" },
    })).toEqual({
      workspace: "Choose at least one workspace type and enter a positive whole-number size, or leave the size blank.",
    });
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
      displayName: "Engineering",
      envelope,
      memberRoles: ["engineer"],
      models: envelope.spec.llms,
      monthlyLimit: "10.00",
      singleRunLimit: "1.00",
      templateId: "engineer",
      tools: [],
      workspace: undefined,
    })).toEqual({
      ttl: "Enter a time-to-live limit.",
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
      displayName: "x".repeat(129),
      envelope,
      memberRoles: Array.from({ length: 65 }, (_, index) => `role-${index}`),
      models: envelope.spec.llms,
      monthlyLimit: "10.00",
      singleRunLimit: "1.00",
      templateId: "engineer",
      tools: [],
      workspace: undefined,
    })).toEqual({
      displayName: "Use at most 128 characters.",
      memberRoles: "Use at most 64 eligible member roles.",
    });
  });
});
