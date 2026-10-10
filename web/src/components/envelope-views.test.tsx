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
    expect(source).toContain("Limit {template.ceiling.spec.runtimeMinutesLimit} minutes");
    expect(source).toContain("Workflow rendering is unavailable. Try again.");
    expect(source).not.toContain("authoritative workflow service");
  });
});

const template: AvailableEnvelopeTemplate = {
  allowInlineBrowserTasks: true,
  ceiling: {
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
  displayName: "Engineer",
  id: "engineer",
  revision: 3,
};

describe("envelope request review summary", () => {
  test("names every visible value above the template limits", () => {
    expect(templateRequestReviewSummary({
      budget: "150.00",
      models: new Set(["provider-a\u0000model-a"]),
      runtimeMinutes: "90",
      template,
      tools: new Set(),
      ttl: "2h",
    })).toEqual({
      exceedances: [
        { ceiling: "50.00 USD", label: "Monthly limit", requested: "150.00 USD" },
        { ceiling: "30 minutes", label: "Runtime minutes", requested: "90 minutes" },
        { ceiling: "30m", label: "Time to live", requested: "2h" },
      ],
      requiresApproval: true,
    });
  });

  test("requires review above the template limits", () => {
    expect(templateRequestReviewSummary({
      budget: "150.00",
      models: new Set(["provider-a\u0000model-a"]),
      runtimeMinutes: "30",
      template,
      tools: new Set(),
      ttl: "30m",
    })).toEqual({
      exceedances: [{ ceiling: "50.00 USD", label: "Monthly limit", requested: "150.00 USD" }],
      requiresApproval: true,
    });
  });

  test("provisions automatically within the template limits", () => {
    expect(templateRequestReviewSummary({
      budget: "40.00",
      models: new Set(["provider-a\u0000model-a"]),
      runtimeMinutes: "20",
      template,
      tools: new Set(),
      ttl: "20m",
    })).toEqual({ exceedances: [], requiresApproval: false });
  });
});
