import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { renderToStaticMarkup } from "react-dom/server";

import type { BrowserEnvelopeRequestView } from "@/api-client";

import { EnvelopeRequestCard } from "./admin-approvals-view";

const source = readFileSync(new URL("./admin-approvals-view.tsx", import.meta.url), "utf8");

const request: BrowserEnvelopeRequestView = {
  createdAt: "2026-08-30T12:00:00Z",
  ownerDisplayEmail: "alice@example.com",
  requestId: "00000000-0000-0000-0000-000000000004",
  requestedEnvelope: {
    revision: 7,
    spec: {
      budget: { currency: "USD", monthlyLimit: "300.00", singleRunLimit: "30.00" },
      llms: [{ model: "model-b", provider: "provider-a" }],
      runner: { compute: "2000m", memory: "8Gi", platforms: ["linux", "mac"], storage: "20Gi" },
      tools: [],
      ttl: "36h",
    },
  },
  templateEnvelope: {
    revision: 7,
    spec: {
      budget: { currency: "USD", monthlyLimit: "200.00", singleRunLimit: "20.00" },
      llms: [{ model: "model-a", provider: "provider-a" }],
      runner: { compute: "1000m", memory: "4Gi", platforms: ["linux"], storage: "10Gi" },
      tools: [],
      ttl: "24h",
    },
  },
  templateId: "engineer",
  templateRevision: 7,
};

describe("envelope request approval controls", () => {
  test("offer governed approve and reject mutations with an optional rejection reason", () => {
    const html = renderToStaticMarkup(<EnvelopeRequestCard request={request} />);

    expect(source).toContain("approveAdminEnvelopeRequest");
    expect(source).toContain("rejectAdminEnvelopeRequest");
    expect(html).toContain("Approve request");
    expect(html).toContain("Reject request");
    expect(html).toContain('name="reason"');
    expect(html).toContain("Rejection reason (optional)");
  });

  test("show the requested authority beside its exact governing template revision", () => {
    const html = renderToStaticMarkup(<EnvelopeRequestCard request={request} />);

    expect(html).toContain("Requested authority");
    expect(html).toContain("Governing template");
    expect(html).toContain("300.00 USD");
    expect(html).toContain("200.00 USD");
    expect(html).toContain("model-b");
    expect(html).toContain("model-a");
    expect(html).toContain("Single-run limit");
    expect(html).toContain("30.00 USD");
    expect(html).toContain("20.00 USD");
    expect(html).toContain("Runner platforms");
    expect(html).toContain("linux, mac");
    expect(html).toContain("Runner memory");
    expect(html).toContain("8Gi");
    expect(html).toContain("4Gi");
    expect(html).toContain("Runner compute");
    expect(html).toContain("2000m");
    expect(html).toContain("1000m");
    expect(html).toContain("Runner storage");
    expect(html).toContain("20Gi");
    expect(html).toContain("10Gi");
    expect(html).toContain("Template revision");
    expect(html).toContain(">7<");
  });

  test("labels an absent requested per-run limit as unbounded authority", () => {
    const unboundedRequest: BrowserEnvelopeRequestView = {
      ...request,
      requestedEnvelope: {
        ...request.requestedEnvelope,
        spec: {
          ...request.requestedEnvelope.spec,
          budget: { ...request.requestedEnvelope.spec.budget, singleRunLimit: null },
        },
      },
    };

    expect(renderToStaticMarkup(<EnvelopeRequestCard request={unboundedRequest} />)).toContain("Unbounded");
  });

  test("does not classify a failed rejection as a completed rejection", () => {
    expect(source).toContain('"rejection-complete"');
    expect(source).toContain('status === "rejection-complete"');
  });
});
