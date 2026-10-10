import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import type { AvailableEnvelopeTemplate } from "@/api-client";
import { templateRequestReviewSummary } from "./envelope-views";

const source = readFileSync(new URL("./envelope-views.tsx", import.meta.url), "utf8");

describe("envelope run history", () => {
  test("filters runs by the durable envelope instance rather than the runtime UID", () => {
    expect(source).toContain("query: { envelopeInstanceId }");
    expect(source).not.toContain("query: { runtimeUid }");
  });

  test("uses the approved request copy and explicit units", () => {
    expect(source).toContain("Pick an existing template and adjust it, or create your own envelope request.");
    expect(source).toContain("Limit per run");
    expect(source).toContain("Ceiling {template.ceiling.spec.runtimeMinutesLimit} minutes");
    expect(source).toContain("Workflow rendering is unavailable. Try again.");
    expect(source).not.toContain("authoritative workflow service");
  });
});

const template: AvailableEnvelopeTemplate = {
  allowInlineBrowserTasks: true,
  autoProvisionThreshold: {
    revision: 3,
    spec: {
      budget: { currency: "USD", monthlyLimit: "50.00", singleRunLimit: "5.00" },
      llms: [{ provider: "provider-a", model: "model-a" }],
      tools: [],
      ttl: "30m",
      runner: {},
      runtimeMinutesLimit: "30",
    },
  },
  ceiling: {
    revision: 3,
    spec: {
      budget: { currency: "USD", monthlyLimit: "100.00", singleRunLimit: "10.00" },
      llms: [{ provider: "provider-a", model: "model-a" }],
      tools: [],
      ttl: "1h",
      runner: {},
      runtimeMinutesLimit: "60",
    },
  },
  displayName: "Engineer",
  id: "engineer",
  revision: 3,
};

describe("envelope request review summary", () => {
  test("names every visible value above the template ceiling", () => {
    expect(templateRequestReviewSummary({
      budget: "101.00",
      models: new Set(["provider-a\u0000model-a"]),
      runtimeMinutes: "61",
      template,
      tools: new Set(),
      ttl: "61m",
    })).toEqual({
      exceedances: [
        { ceiling: "100.00 USD", label: "Monthly limit", requested: "101.00 USD" },
        { ceiling: "60 minutes", label: "Runtime minutes", requested: "61 minutes" },
        { ceiling: "1h", label: "Time to live", requested: "61m" },
      ],
      requiresApproval: true,
    });
  });

  test("requires review above a narrower automatic approval threshold", () => {
    expect(templateRequestReviewSummary({
      budget: "75.00",
      models: new Set(["provider-a\u0000model-a"]),
      runtimeMinutes: "30",
      template,
      tools: new Set(),
      ttl: "30m",
    })).toEqual({ exceedances: [], requiresApproval: true });
  });

  test("returns to automatic submission inside the automatic approval threshold", () => {
    expect(templateRequestReviewSummary({
      budget: "25.00",
      models: new Set(["provider-a\u0000model-a"]),
      runtimeMinutes: "20",
      template: {
        ...template,
        autoProvisionThreshold: {
          ...template.autoProvisionThreshold!,
          spec: {
            ...template.autoProvisionThreshold!.spec,
            budget: { ...template.ceiling.spec.budget, monthlyLimit: "50.00" },
          },
        },
      },
      tools: new Set(),
      ttl: "20m",
    })).toEqual({ exceedances: [], requiresApproval: false });
  });
});
