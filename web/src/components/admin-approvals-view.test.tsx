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
      budget: { currency: "USD", monthlyLimit: "300.00" },
      llms: [{ model: "model-b", provider: "provider-a" }],
      tools: [],
      ttl: "36h",
    },
  },
  templateEnvelope: {
    revision: 7,
    spec: {
      budget: { currency: "USD", monthlyLimit: "200.00" },
      llms: [{ model: "model-a", provider: "provider-a" }],
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
    expect(html).toContain("Template revision");
    expect(html).toContain(">7<");
  });
});
