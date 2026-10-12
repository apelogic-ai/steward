import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import type { AvailableEnvelopeTemplate, BrowserEnvelope } from "@/api-client";
import { parseCustomEnvelope, templateRequestReviewSummary, withWorkspaceAuthority } from "./envelope-views";
import { workspaceAuthorityDraft, workspaceAuthorityFromDraft } from "./workspace-authority-controls";

const source = readFileSync(new URL("./envelope-views.tsx", import.meta.url), "utf8");

describe("envelope run history", () => {
  test("filters runs by the durable envelope instance rather than the runtime UID", () => {
    expect(source).toContain("query: { envelopeInstanceId }");
    expect(source).not.toContain("query: { runtimeUid }");
  });

  test("uses the approved request copy and explicit units", () => {
    expect(source).toContain("Pick an existing template and adjust it, or create your own envelope request.");
    expect(source).toContain("Limit per run");
    expect(source).toContain("formatDuration(template.ceiling.spec.runtimeMinutesLimit, \"minutes\", \"m\")");
    expect(source).toContain("formatMoney(spend.observed, spend.currency)");
    expect(source).toContain("Workflow rendering is unavailable. Try again.");
    expect(source).not.toContain("authoritative workflow service");
  });

  test("shows request and history reasons directly because they are human-written", () => {
    expect(source).toContain('request.reason ? <p className="rounded-card');
    expect(source).toContain('event.reason ? <p className="mt-1 text-sm');
  });

  test("explains empty model searches and blocks requests without a model", () => {
    expect(source).toContain("No models in this template match");
    expect(source).toContain("Select at least one model.");
    expect(source).toContain('aria-invalid={models.size === 0 || rejectedFields.includes("llms") || undefined}');
    expect(source).toContain('(requiresApproval && !justification.trim())');
  });

  test("shows workspace access on envelope details", () => {
    expect(source).toContain("WorkspaceAuthorityDetails");
    expect(source).toContain('workspace={envelope.spec.workspace}');
    expect(source).toContain('title={request.approvedEnvelope ? "Approved access" : "Requested access"}');
  });

  test("loads deployment workspace capabilities for template and custom requests", () => {
    expect(source).toContain("getUserCapabilities");
    expect(source).toContain("WorkspaceAuthorityControls");
    expect(source).toContain("capabilitiesState.value.workspace");
  });

  test("custom requests preserve legacy narrowing limits while updating the structured selection", () => {
    const sourceEnvelope: BrowserEnvelope = {
      revision: 1,
      spec: {
        budget: { currency: "USD", monthlyLimit: "20.00" },
        llms: [{ provider: "provider-a", model: "model-a" }],
        tools: [],
        ttl: "30m",
        workspace: {
          allowedTypes: ["git", "scratch"],
          maxFiles: 1_000,
          maxHistoryDepth: 50,
          maxSubmoduleDepth: 2,
          maxTotalSize: "4Gi",
        },
      },
    };
    const draft = workspaceAuthorityDraft(sourceEnvelope.spec.workspace, ["git", "scratch"]);
    const workspace = workspaceAuthorityFromDraft({ ...draft, allowedTypes: ["git"], size: "2" }, ["git", "scratch"]);
    expect(workspace).not.toBeNull();
    const envelope = withWorkspaceAuthority(sourceEnvelope, workspace ?? undefined);

    expect(envelope.spec.workspace).toEqual({
      allowedTypes: ["git"],
      maxFiles: 1_000,
      maxHistoryDepth: 50,
      maxSubmoduleDepth: 2,
      maxTotalSize: "2Gi",
    });
  });

  test("custom requests reject malformed workspace authority before editing it", () => {
    expect(parseCustomEnvelope(JSON.stringify({
      revision: 1,
      spec: {
        budget: { currency: "USD", monthlyLimit: "20.00" },
        llms: [{ provider: "provider-a", model: "model-a" }],
        tools: [],
        ttl: "30m",
        workspace: { maxTotalSize: "2Gi" },
      },
    }))).toBeNull();
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
      singleRunLimit: "11.00",
      runtimeMinutes: "90",
      template,
      tools: new Set(),
      ttl: "2h",
    })).toEqual({
      exceedances: [
        { ceiling: "50.00 USD", label: "Monthly limit", requested: "150.00 USD" },
        { ceiling: "5.00 USD", label: "Limit per run", requested: "11.00 USD" },
        { ceiling: "30 min", label: "Runtime limit per month", requested: "90 min" },
        { ceiling: "0.5 h", label: "Time-to-live limit", requested: "2 h" },
      ],
      requiresApproval: true,
    });
  });

  test("requires review above the template limits", () => {
    expect(templateRequestReviewSummary({
      budget: "150.00",
      models: new Set(["provider-a\u0000model-a"]),
      singleRunLimit: "5.00",
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
      singleRunLimit: "2.50",
      runtimeMinutes: "20",
      template,
      tools: new Set(),
      ttl: "20m",
    })).toEqual({ exceedances: [], requiresApproval: false });
  });

  test("uses the requested per-run limit for automatic approval", () => {
    expect(templateRequestReviewSummary({
      budget: "25.00",
      models: new Set(["provider-a\u0000model-a"]),
      singleRunLimit: "4.00",
      runtimeMinutes: "20",
      template,
      tools: new Set(),
      ttl: "20m",
    })).toEqual({ exceedances: [], requiresApproval: false });
  });
});
