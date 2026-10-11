import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { renderToStaticMarkup } from "react-dom/server";

import type { AdminRequestView, BrowserEnvelopeRequestView } from "@/api-client";

import { EnvelopeRequestCard, UnifiedRequestCard, requestReason, triggerLabel } from "./admin-approvals-view";

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

const unifiedRequest: AdminRequestView = {
  createdAt: "2026-08-30T12:00:00Z",
  deltas: [],
  history: [],
  id: request.requestId,
  kind: "within_ceiling",
  requester: {
    displayEmail: request.ownerDisplayEmail,
    userId: "user-alice",
  },
  source: "envelope_request",
  state: "requested",
  stateActor: "alice",
  stateAt: "2026-08-30T12:00:00Z",
  template: {
    displayName: "Engineering",
    id: request.templateId,
    revision: request.templateRevision,
  },
};

describe("envelope request approval controls", () => {
  test("uses plain request wording and consistent per-run labels", () => {
    expect(source).toContain("Limit per run");
    expect(source).toContain("custom envelope requests");
    expect(source).not.toContain("Reload the authoritative queue");
    expect(source).not.toContain("request authority is unavailable");
  });

  test("unified queue preserves rejection and optional approval evidence", () => {
    const html = renderToStaticMarkup(<UnifiedRequestCard request={unifiedRequest} />);

    expect(html).toContain("File a decision reference");
    expect(html).toContain("Approve or reject");
    expect(html).toContain(">Reject</button>");
    expect(html).toContain('name="reason"');
    expect(html).toContain("Expires at (optional)");
    expect(html).not.toContain('name="expiresAt" placeholder="2026-08-25T17:00:00Z" required=""');
  });

  test("describes a completed within-limits request from its status", () => {
    const completed: AdminRequestView = {
      ...unifiedRequest,
      history: [{ actor: "system:auto", at: "2026-08-30T12:00:01Z", state: "auto_approved" }],
      state: "auto_approved",
      stateActor: "system:auto",
      stateAt: "2026-08-30T12:00:01Z",
    };

    expect(requestReason(completed)).toBe("auto-approved");
    expect(triggerLabel(completed)).toEqual(["Auto-approved", "Provisioned automatically"]);
    const html = renderToStaticMarkup(<UnifiedRequestCard request={completed} />);
    expect(html).toContain("This request was provisioned automatically.");
    expect(html).not.toContain("awaiting administrator approval");
  });

  test("does not label decided within-limits requests as awaiting review", () => {
    const cases = [
      ["approved", "approved", "Approved", "Approved by an administrator"],
      ["rejected", "rejected", "Rejected", "Request was rejected"],
      ["expired", "expired", "Expired", "Request expired"],
    ] as const;

    for (const [state, reason, label, detail] of cases) {
      const decided: AdminRequestView = { ...unifiedRequest, state };
      expect(requestReason(decided)).toBe(reason);
      expect(triggerLabel(decided)).toEqual([label, detail]);
    }
  });

  test("includes field names and units in above-limit trigger details", () => {
    const aboveLimits: AdminRequestView = {
      ...unifiedRequest,
      deltas: [
        { ceiling: "50.00", currency: "EUR", dimension: "budget", requested: "150.00" },
        { ceiling: "60", dimension: "runtimeMinutes", requested: "90" },
        { ceiling: "2h", dimension: "ttl", requested: "3h" },
      ],
      kind: "ceiling_exceeded",
    };

    expect(triggerLabel(aboveLimits)).toEqual([
      "Above limits",
      "Monthly limit: 150.00 EUR > 50.00 EUR · Runtime limit per month: 90 min > 60 min · Time-to-live limit: 3 h > 2 h",
    ]);
  });

  test("shows the requester justification beside recorded limit deltas", () => {
    const html = renderToStaticMarkup(<UnifiedRequestCard request={{
      ...unifiedRequest,
      deltas: [{ ceiling: "200.00", currency: "USD", dimension: "budget", requested: "300.00" }],
      justification: "Needed for a release review",
      kind: "ceiling_exceeded",
    }} />);

    expect(html).toContain("Requester justification");
    expect(html).toContain("Needed for a release review");
    expect(html).toContain("Requested vs template limits");
    expect(html).toContain("Needs admin approval: 1 value above the template&#x27;s limits");
    expect(html).toContain("Monthly limit: <strong>300.00 USD</strong> (limit 200.00 USD)");
    expect(html).toContain("300.00 USD");
    expect(html).toContain("200.00 USD");
  });

  test("shows a human-written history reason without hiding it as a diagnostic", () => {
    const html = renderToStaticMarkup(<UnifiedRequestCard request={{
      ...unifiedRequest,
      history: [{ actor: "alice", at: "2026-08-30T12:01:00Z", reason: "Needed for the release review", state: "requested" }],
      state: "rejected",
    }} />);

    expect(html).toContain("Needed for the release review");
    expect(html).not.toContain("<summary");
  });

  test("shows the full requested access for a template-free custom request", () => {
    const custom: AdminRequestView = {
      ...unifiedRequest,
      kind: "custom",
      requestedEnvelope: request.requestedEnvelope,
      template: {},
    };

    const html = renderToStaticMarkup(<UnifiedRequestCard request={custom} />);

    expect(html).toContain("Requested access");
    expect(html).toContain("300.00 USD");
    expect(html).toContain("provider-a/model-b");
    expect(html).toContain("1 d 12 h");
  });

  test("offer governed approve and reject mutations with an optional rejection reason", () => {
    const html = renderToStaticMarkup(<EnvelopeRequestCard request={request} />);

    expect(source).toContain("approveAdminEnvelopeRequest");
    expect(source).toContain("rejectAdminEnvelopeRequest");
    expect(html).toContain("Approve request");
    expect(html).toContain("Reject request");
    expect(html).toContain('name="reason"');
    expect(html).toContain("Rejection reason (optional)");
  });

  test("renders a template-free custom request without inventing template authority", () => {
    const custom: BrowserEnvelopeRequestView = {
      ...request,
      templateEnvelope: null,
      templateId: null,
      templateRevision: null,
    };

    const html = renderToStaticMarkup(<EnvelopeRequestCard request={custom} />);

    expect(html).toContain("Custom");
    expect(html).toContain("not auto-provisioned");
  });

  test("shows the requested access beside its exact governing template revision", () => {
    const html = renderToStaticMarkup(<EnvelopeRequestCard request={request} />);

    expect(html).toContain("Requested access");
    expect(html).toContain("Governing template");
    expect(html).toContain("300.00 USD");
    expect(html).toContain("200.00 USD");
    expect(html).toContain("model-b");
    expect(html).toContain("model-a");
    expect(html).toContain("Limit per run");
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
