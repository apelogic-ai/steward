"use client";

import { useCallback, useState, type FormEvent } from "react";

import {
  approveAdminApproval,
  approveAdminEnvelopeRequest,
  denyAdminEscalation,
  fileAdminApprovalDecision,
  fileAdminEnvelopeRequest,
  listAdminRequests,
  topUpAdminEscalation,
  rejectAdminEnvelopeRequest,
  type BrowserApprovalView,
  type BrowserDecisionReferenceResponse,
  type BrowserEnvelopeSpec,
  type BrowserEnvelopeRequestDecisionResponse,
  type BrowserEnvelopeRequestView,
  type AdminRequestView,
  type AdminRequestsResponse,
} from "@/api-client";
import { DefinitionList, EmptyState, PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { classifyMutationFailure } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

type ApprovalActionState = "idle" | "filing" | "filed" | "approving" | "approved" | "conflict" | "rejected" | "forbidden" | "unavailable" | "error";
type UnifiedRequestActionState = ApprovalActionState | "rejecting" | "rejection-complete";
type EnvelopeActionState = "idle" | "approving" | "rejecting" | "provisioned" | "rejection-complete" | "rejected" | "conflict" | "forbidden" | "unavailable" | "error";

function actionMessage(status: Exclude<UnifiedRequestActionState, "idle" | "filing" | "approving" | "rejecting">): string {
  return {
    filed: "Decision reference filed through the server-owned channel.",
    approved: "Approval applied through the governed Rust admission path.",
    "rejection-complete": "The envelope request was rejected through the governed Rust admission path.",
    conflict: "This approval or its runtime is stale. Reload the authoritative queue.",
    rejected: "The approval evidence or expiry is invalid.",
    forbidden: "The Rust authorization boundary rejected this mutation.",
    unavailable: "The approval authority is unavailable.",
    error: "The approval response could not be accepted.",
  }[status];
}

function envelopeAuthorityItems(spec: BrowserEnvelopeSpec): [string, string][] {
  const runner = spec.runner;
  return [
    ["Models", spec.llms.map((model) => `${model.provider}/${model.model}`).join(", ") || "None"],
    ["Tools", spec.tools.map((tool) => `${tool.provider}/${tool.resource}:${tool.action}`).join(", ") || "None"],
    ["Monthly limit", `${spec.budget.monthlyLimit} ${spec.budget.currency}`],
    ["Single-run limit", spec.budget.singleRunLimit
      ? `${spec.budget.singleRunLimit} ${spec.budget.currency}`
      : "Unbounded"],
    ["TTL", spec.ttl],
    ["Runner platforms", runner?.platforms?.join(", ") || "None"],
    ["Runner memory", runner?.memory ?? "Not set"],
    ["Runner compute", runner?.compute ?? "Not set"],
    ["Runner storage", runner?.storage ?? "Not set"],
  ];
}

export function AdminApprovalsView() {
  const load = useCallback(() => listAdminRequests({ cache: "no-store", credentials: "same-origin", query: { state: "needs_action", limit: 100 } }), []);
  const state = useApiResource<AdminRequestsResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Review one authoritative queue of envelope requests, runtime exceptions, and cumulative-limit escalations." title="Requests" />
      <ResourceBoundary state={state}>{({ requests }) => requests.length === 0 ? (
        <EmptyState title="No data" />
      ) : (
        <ul className="space-y-5">
          {requests.map((request) => <UnifiedRequestCard key={request.id} request={request} />)}
        </ul>
      )}</ResourceBoundary>
    </section>
  );
}

function deltaValue(value: unknown): string {
  if (value === null || value === undefined) return "Not set";
  if (typeof value === "string" || typeof value === "number") return String(value);
  return JSON.stringify(value);
}

export function UnifiedRequestCard({ request }: Readonly<{ request: AdminRequestView }>) {
  const session = useSession();
  const [reference, setReference] = useState<{ decisionKey: string; evidenceUrl: string } | null>(request.decision?.decisionKey && request.decision.evidenceUrl ? {
    decisionKey: request.decision.decisionKey,
    evidenceUrl: request.decision.evidenceUrl,
  } : null);
  const [status, setStatus] = useState<UnifiedRequestActionState>("idle");

  async function fileDecision() {
    if (session.status !== "authenticated") return;
    setStatus("filing");
    const options = { body: {}, cache: "no-store" as const, credentials: "same-origin" as const, headers: { "X-Steward-CSRF": session.value.csrf } };
    const result = request.source === "envelope_request"
      ? await fileAdminEnvelopeRequest({ ...options, path: { request_id: request.id } })
      : await fileAdminApprovalDecision({ ...options, path: { approval_id: request.id } });
    if (result.data && result.response?.ok) {
      setReference({ decisionKey: result.data.decisionKey, evidenceUrl: result.data.evidenceUrl });
      setStatus("filed");
    } else setStatus(classifyMutationFailure(result.response?.status));
  }

  async function approve(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated") return;
    const fields = new FormData(event.currentTarget);
    const rationale = String(fields.get("rationale") ?? "").trim();
    const expiresAt = String(fields.get("expiresAt") ?? "").trim();
    setStatus("approving");
    const result = request.source === "envelope_request"
      ? await approveAdminEnvelopeRequest({
          body: { evidenceUrl: reference?.evidenceUrl ?? null, expiresAt: expiresAt || null, rationale },
          cache: "no-store",
          credentials: "same-origin",
          headers: { "X-Steward-CSRF": session.value.csrf },
          path: { request_id: request.id },
        })
      : reference
        ? await approveAdminApproval({
            body: { evidenceUrl: reference.evidenceUrl, expiresAt, rationale },
            cache: "no-store",
            credentials: "same-origin",
            headers: { "X-Steward-CSRF": session.value.csrf },
            path: { approval_id: request.id },
          })
        : null;
    if (!result) {
      setStatus("error");
      return;
    }
    if (result.response?.ok) setStatus("approved");
    else setStatus(classifyMutationFailure(result.response?.status));
  }

  async function rejectEnvelope(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated" || request.source !== "envelope_request") return;
    const fields = new FormData(event.currentTarget);
    const reason = String(fields.get("reason") ?? "").trim();
    setStatus("rejecting");
    const result = await rejectAdminEnvelopeRequest({
      body: { reason: reason || null },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
      path: { request_id: request.id },
    });
    setStatus(result.data && result.response?.ok ? "rejection-complete" : classifyMutationFailure(result.response?.status));
  }

  async function topUp(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated") return;
    const meter = request.escalation?.meters[0];
    if (!meter) return;
    const fields = new FormData(event.currentTarget);
    setStatus("approving");
    const result = await topUpAdminEscalation({
      body: {
        dimension: meter.dimension,
        amount: String(fields.get("amount") ?? "").trim(),
        validUntil: String(fields.get("validUntil") ?? "").trim(),
        rationale: String(fields.get("rationale") ?? "").trim(),
      },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
      path: { escalation_id: request.id },
    });
    setStatus(result.data && result.response?.ok ? "approved" : classifyMutationFailure(result.response?.status));
  }

  async function deny(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated") return;
    const fields = new FormData(event.currentTarget);
    setStatus("approving");
    const result = await denyAdminEscalation({
      body: { rationale: String(fields.get("denyRationale") ?? "").trim() },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
      path: { escalation_id: request.id },
    });
    setStatus(result.data && result.response?.ok ? "rejected" : classifyMutationFailure(result.response?.status));
  }

  if (request.source === "escalation" && request.escalation) {
    return (
      <li className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm">
        <div className="flex flex-wrap items-start justify-between gap-4"><div><h2 className="text-xl font-semibold">{request.template.displayName}</h2><p className="mt-1 break-all font-mono text-xs text-muted-ink">Escalation {request.id}</p></div><StatusBadge value={status === "approved" ? "approved" : status === "rejected" ? "rejected" : request.state} /></div>
        <DefinitionList items={[["Requested by", request.requester.displayEmail], ["Envelope instance", request.escalation.envelopeInstanceId], ["Blocked task", request.escalation.blockedTaskUid], ["Parked", request.escalation.parkedAt]]} />
        <ul className="space-y-2">{request.escalation.meters.map((meter) => <li className="rounded-md border p-4" key={meter.dimension}><p className="font-semibold">{meter.dimension === "llm_spend" ? "LLM spend" : "Runtime minutes"}</p><p className="mt-1 text-sm">{meter.used} / {meter.limit} {meter.unit}</p><p className="mt-1 text-xs text-muted-ink">Observed {meter.observedAt}</p></li>)}</ul>
        <form className="grid gap-3 border-t pt-5 sm:grid-cols-2" onSubmit={topUp}>
          <label className="grid gap-2 text-sm font-semibold">Top-up amount ({request.escalation.meters[0]?.unit ?? "units"})<input className="min-h-11 rounded-md border px-3 font-normal" inputMode="decimal" name="amount" required /></label>
          <label className="grid gap-2 text-sm font-semibold">Valid until (RFC 3339)<input className="min-h-11 rounded-md border px-3 font-normal" name="validUntil" placeholder="2026-08-25T17:00:00Z" required /></label>
          <label className="grid gap-2 text-sm font-semibold sm:col-span-2">Rationale<textarea className="min-h-20 rounded-md border p-3 font-normal" name="rationale" required /></label>
          <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50 sm:justify-self-start" disabled={status === "approving" || status === "approved" || status === "rejected"} type="submit">Top up and resume</button>
        </form>
        <form className="grid gap-3" onSubmit={deny}>
          <label className="grid gap-2 text-sm font-semibold">Denial rationale<textarea className="min-h-20 rounded-md border p-3 font-normal" name="denyRationale" required /></label>
          <button className="min-h-11 rounded-md border border-red-700 px-4 py-2 text-sm font-semibold text-red-800 disabled:opacity-50 sm:justify-self-start" disabled={status === "approving" || status === "approved" || status === "rejected"} type="submit">Deny and cancel task</button>
        </form>
      </li>
    );
  }

  return (
    <li className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm">
      <div className="flex flex-wrap items-start justify-between gap-4"><div><h2 className="text-xl font-semibold">{request.template.displayName}</h2><p className="mt-1 break-all font-mono text-xs text-muted-ink">{request.id}</p></div><StatusBadge value={status === "approved" ? "approved" : status === "rejection-complete" ? "rejected" : request.state} /></div>
      <DefinitionList items={[["Kind", request.kind], ["Source", request.source], ["Requested by", request.requester.displayEmail], ["Created", request.createdAt], ["State actor", request.stateActor]]} />
      {request.deltas.length ? <section className="space-y-3"><h3 className="font-semibold">Requested changes</h3><ul className="space-y-2">{request.deltas.map((delta, index) => <li className="rounded-md border p-3 text-sm" key={`${delta.dimension}-${index}`}><strong>{delta.dimension}</strong>: {deltaValue(delta.requested)} <span className="text-muted-ink">(ceiling {deltaValue(delta.ceiling)})</span></li>)}</ul></section> : <p className="rounded-md bg-notice p-4 text-sm">This request is within the configured ceiling.</p>}
      {reference ? <DefinitionList items={[["Decision key", reference.decisionKey], ["Evidence URL", reference.evidenceUrl]]} /> : <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={status === "filing"} onClick={() => void fileDecision()} type="button">{status === "filing" ? "Filing…" : "File decision reference"}</button>}
      <form className="grid gap-4 border-t pt-5 sm:grid-cols-2" onSubmit={approve}>
        <label className="grid gap-2 text-sm font-semibold sm:col-span-2">Rationale<textarea className="min-h-24 rounded-md border p-3 font-normal" name="rationale" required /></label>
        <label className="grid gap-2 text-sm font-semibold">Expires at{request.source === "envelope_request" ? " (optional)" : " (RFC 3339)"}<input className="min-h-11 rounded-md border px-3 font-normal" name="expiresAt" placeholder="2026-08-25T17:00:00Z" required={request.source !== "envelope_request"} /></label>
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50 sm:self-end" disabled={(request.source !== "envelope_request" && !reference) || status === "approving" || status === "rejecting" || status === "approved" || status === "rejection-complete"} type="submit">{status === "approving" ? "Approving…" : status === "approved" ? "Approved" : "Approve"}</button>
      </form>
      {request.source === "envelope_request" ? <form className="grid gap-3" onSubmit={rejectEnvelope}>
        <label className="grid gap-2 text-sm font-semibold">Rejection reason (optional)<textarea className="min-h-20 rounded-md border p-3 font-normal" maxLength={2000} name="reason" /></label>
        <button className="min-h-11 rounded-md border border-red-700 px-4 py-2 text-sm font-semibold text-red-800 disabled:opacity-50 sm:justify-self-start" disabled={status === "approving" || status === "rejecting" || status === "approved" || status === "rejection-complete"} type="submit">{status === "rejecting" ? "Rejecting…" : "Reject request"}</button>
      </form> : null}
      {status !== "idle" && status !== "filing" && status !== "approving" && status !== "rejecting" ? <p className={status === "approved" || status === "filed" || status === "rejection-complete" ? "text-sm text-green-800" : "text-sm text-red-800"} role={status === "approved" || status === "filed" || status === "rejection-complete" ? "status" : "alert"}>{actionMessage(status)}</p> : null}
    </li>
  );
}

export function EnvelopeRequestCard({ request }: Readonly<{ request: BrowserEnvelopeRequestView }>) {
  const session = useSession();
  const [decision, setDecision] = useState<BrowserEnvelopeRequestDecisionResponse | null>(null);
  const [status, setStatus] = useState<EnvelopeActionState>("idle");
  const requested = request.requestedEnvelope.spec;
  const governing = request.templateEnvelope.spec;
  const terminal = decision?.request.status === "provisioned" || decision?.request.status === "rejected";

  async function approveRequest(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated" || terminal) return;
    const fields = new FormData(event.currentTarget);
    const expiresAt = String(fields.get("expiresAt") ?? "").trim();
    setStatus("approving");
    const result = await approveAdminEnvelopeRequest({
      body: {
        rationale: String(fields.get("rationale") ?? "").trim(),
        evidenceUrl: String(fields.get("evidenceUrl") ?? "").trim() || null,
        expiresAt: expiresAt ? new Date(expiresAt).toISOString() : null,
      },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
      path: { request_id: request.requestId },
    });
    if (result.data && result.response?.status === 200) {
      setDecision(result.data);
      setStatus("provisioned");
      return;
    }
    setStatus(classifyMutationFailure(result.response?.status));
  }

  async function rejectRequest(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated" || terminal) return;
    const fields = new FormData(event.currentTarget);
    const reason = String(fields.get("reason") ?? "").trim();
    setStatus("rejecting");
    const result = await rejectAdminEnvelopeRequest({
      body: { reason: reason || null },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
      path: { request_id: request.requestId },
    });
    if (result.data && result.response?.status === 200) {
      setDecision(result.data);
      setStatus("rejection-complete");
      return;
    }
    setStatus(classifyMutationFailure(result.response?.status));
  }

  return (
    <li className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div><h2 className="text-xl font-semibold">User envelope request</h2><p className="mt-1 break-all font-mono text-xs text-muted-ink">{request.requestId}</p></div>
        <StatusBadge value={decision?.request.status ?? "pending"} />
      </div>
      <DefinitionList items={[
        ["Requested by", request.ownerDisplayEmail],
        ["Template", request.templateId],
        ["Template revision", request.templateRevision],
        ["Created", request.createdAt],
      ]} />
      <div className="grid gap-5 border-t pt-5 lg:grid-cols-2">
        <section aria-label="Requested authority" className="space-y-3">
          <h3 className="font-semibold">Requested authority</h3>
          <DefinitionList items={envelopeAuthorityItems(requested)} />
        </section>
        <section aria-label="Governing template" className="space-y-3">
          <h3 className="font-semibold">Governing template</h3>
          <DefinitionList items={envelopeAuthorityItems(governing)} />
        </section>
      </div>
      <form className="grid gap-3 border-t pt-5" onSubmit={approveRequest}>
        <label className="grid gap-2 text-sm font-semibold">Approval rationale
          <textarea className="min-h-20 rounded-md border p-3 font-normal" disabled={terminal} maxLength={2000} name="rationale" required />
        </label>
        <div className="grid gap-3 sm:grid-cols-2">
          <label className="grid gap-2 text-sm font-semibold">Evidence URL (optional)
            <input className="min-h-11 rounded-md border px-3 font-normal" disabled={terminal} name="evidenceUrl" placeholder="https://…" type="url" />
          </label>
          <label className="grid gap-2 text-sm font-semibold">Expires at (optional)
            <input className="min-h-11 rounded-md border px-3 font-normal" disabled={terminal} name="expiresAt" type="datetime-local" />
          </label>
        </div>
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50 sm:justify-self-start" disabled={terminal || status === "approving" || status === "rejecting"} type="submit">{status === "approving" ? "Approving…" : "Approve request"}</button>
      </form>
      <form className="grid gap-3" onSubmit={rejectRequest}>
        <label className="grid gap-2 text-sm font-semibold">Rejection reason (optional)
          <textarea className="min-h-20 rounded-md border p-3 font-normal" disabled={terminal} maxLength={2000} name="reason" />
        </label>
        <button className="min-h-11 rounded-md border border-red-700 px-4 py-2 text-sm font-semibold text-red-800 disabled:opacity-50 sm:justify-self-start" disabled={terminal || status === "approving" || status === "rejecting"} type="submit">{status === "rejecting" ? "Rejecting…" : "Reject request"}</button>
      </form>
      {decision ? (
        <DefinitionList items={[
          ["Acted by", decision.request.actedBy],
          ["Status time", decision.request.statusAt],
          ["Envelope ID", decision.request.envelopeInstanceId ?? "Not created"],
          ["Reason", decision.request.reason ?? "None"],
        ]} />
      ) : null}
      {status !== "idle" && status !== "approving" && status !== "rejecting" ? (
        <p className={status === "provisioned" || status === "rejection-complete" ? "text-sm text-green-800" : "text-sm text-red-800"} role={status === "provisioned" || status === "rejection-complete" ? "status" : "alert"}>
          {status === "provisioned" ? "The exact requested envelope was provisioned." : status === "rejection-complete" ? "The envelope request was rejected without creating an envelope." : status === "rejected" ? "The rejection reason is invalid." : status === "conflict" ? "This request or its template revision is stale. Reload the authoritative queue." : status === "forbidden" ? "The Rust authorization boundary rejected this mutation." : status === "unavailable" ? "The envelope request authority is unavailable." : "The envelope request response could not be accepted."}
        </p>
      ) : null}
    </li>
  );
}

export function ApprovalCard({ approval }: Readonly<{ approval: BrowserApprovalView }>) {
  const session = useSession();
  const [reference, setReference] = useState<BrowserDecisionReferenceResponse | null>(approval.decisionKey && approval.evidenceUrl ? {
    apiVersion: "steward.browser-admin/v1",
    approvalId: approval.approvalId,
    decisionKey: approval.decisionKey,
    evidenceUrl: approval.evidenceUrl,
  } : null);
  const [status, setStatus] = useState<ApprovalActionState>("idle");
  const spec = approval.proposedSpec;

  async function fileDecision() {
    if (session.status !== "authenticated") return;
    setStatus("filing");
    const result = await fileAdminApprovalDecision({
      body: {},
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
      path: { approval_id: approval.approvalId },
    });
    if (result.data && result.response?.status === 200) {
      setReference(result.data);
      setStatus("filed");
      return;
    }
    setStatus(classifyMutationFailure(result.response?.status));
  }

  async function approve(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated" || !reference) return;
    const fields = new FormData(event.currentTarget);
    setStatus("approving");
    const result = await approveAdminApproval({
      body: {
        evidenceUrl: reference.evidenceUrl,
        expiresAt: String(fields.get("expiresAt") ?? "").trim(),
        rationale: String(fields.get("rationale") ?? "").trim(),
      },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
      path: { approval_id: approval.approvalId },
    });
    if (result.response?.status === 204) {
      setStatus("approved");
      return;
    }
    setStatus(classifyMutationFailure(result.response?.status));
  }

  return (
    <li className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div><h2 className="text-xl font-semibold">{approval.memberRole}</h2><p className="mt-1 break-all font-mono text-xs text-muted-ink">{approval.approvalId}</p></div>
        <StatusBadge value={status === "approved" ? "approved" : "pending"} />
      </div>
      <DefinitionList items={[
        ["Runtime UID", approval.runtimeUid],
        ["Requested by", approval.actor],
        ["Envelope revision", approval.envelopeRevision],
        ["Budget", `${spec.budget.monthlyLimit} ${spec.budget.currency}`],
        ["TTL", spec.ttl],
        ["Agent type", spec.agentType.name],
      ]} />
      <p className="rounded-md bg-notice p-4 text-sm"><strong>Admission counterexample:</strong> {approval.counterexample}</p>
      {reference ? (
        <DefinitionList items={[["Decision key", reference.decisionKey], ["Evidence URL", reference.evidenceUrl]]} />
      ) : (
        <div className="space-y-2 border-t pt-5">
          <p className="text-sm text-muted-ink">This exception has no server-recorded decision reference yet.</p>
          <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={status === "filing"} onClick={() => void fileDecision()} type="button">{status === "filing" ? "Filing…" : "File decision reference"}</button>
        </div>
      )}
      <form className="grid gap-4 border-t pt-5 sm:grid-cols-2" onSubmit={approve}>
        <label className="grid gap-2 text-sm font-semibold sm:col-span-2">Rationale
          <textarea className="min-h-24 rounded-md border p-3 font-normal" name="rationale" required />
        </label>
        <label className="grid gap-2 text-sm font-semibold">Expires at (RFC 3339)
          <input className="min-h-11 rounded-md border px-3 font-normal" name="expiresAt" placeholder="2026-08-25T17:00:00Z" required />
        </label>
        <label className="grid gap-2 text-sm font-semibold">Evidence URL
          <input className="min-h-11 rounded-md border bg-canvas px-3 font-normal" readOnly value={reference?.evidenceUrl ?? "Not filed"} />
        </label>
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50 sm:col-span-2 sm:justify-self-start" disabled={!reference || status === "approving" || status === "approved"} type="submit">{status === "approving" ? "Approving…" : status === "approved" ? "Approved" : "Approve exception"}</button>
      </form>
      {status !== "idle" && status !== "filing" && status !== "approving" ? (
        <p className={status === "approved" || status === "filed" ? "text-sm text-green-800" : "text-sm text-red-800"} role={status === "approved" || status === "filed" ? "status" : "alert"}>{actionMessage(status)}</p>
      ) : null}
    </li>
  );
}
