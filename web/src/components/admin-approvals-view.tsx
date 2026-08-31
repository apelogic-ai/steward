"use client";

import { useCallback, useState, type FormEvent } from "react";

import {
  approveAdminApproval,
  approveAdminEnvelopeRequest,
  fileAdminApprovalDecision,
  listAdminApprovals,
  rejectAdminEnvelopeRequest,
  type BrowserApprovalsResponse,
  type BrowserApprovalView,
  type BrowserDecisionReferenceResponse,
  type BrowserEnvelopeRequestDecisionResponse,
  type BrowserEnvelopeRequestView,
} from "@/api-client";
import { DefinitionList, EmptyState, PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { classifyMutationFailure } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

type ApprovalActionState = "idle" | "filing" | "filed" | "approving" | "approved" | "conflict" | "rejected" | "forbidden" | "unavailable" | "error";
type EnvelopeActionState = "idle" | "approving" | "rejecting" | "provisioned" | "rejected" | "conflict" | "forbidden" | "unavailable" | "error";

type BrowserApprovalQueueResponse = BrowserApprovalsResponse & {
  envelopeRequests: BrowserEnvelopeRequestView[];
};

function actionMessage(status: Exclude<ApprovalActionState, "idle" | "filing" | "approving">): string {
  return {
    filed: "Decision reference filed through the server-owned channel.",
    approved: "Approval applied through the governed Rust admission path.",
    conflict: "This approval or its runtime is stale. Reload the authoritative queue.",
    rejected: "The approval evidence or expiry is invalid.",
    forbidden: "The Rust authorization boundary rejected this mutation.",
    unavailable: "The approval authority is unavailable.",
    error: "The approval response could not be accepted.",
  }[status];
}

export function AdminApprovalsView() {
  const load = useCallback(async () => {
    const result = await listAdminApprovals({ cache: "no-store", credentials: "same-origin" });
    const data = result.data as (BrowserApprovalsResponse & { envelopeRequests?: unknown }) | undefined;
    return {
      ...result,
      data: data ? {
        ...data,
        envelopeRequests: Array.isArray(data.envelopeRequests) ? data.envelopeRequests as BrowserEnvelopeRequestView[] : [],
      } : undefined,
    };
  }, []);
  const state = useApiResource<BrowserApprovalQueueResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Review pending user envelope requests and parked runtime exceptions." title="Pending approvals" />
      <ResourceBoundary state={state}>{({ approvals, envelopeRequests }) => approvals.length === 0 && envelopeRequests.length === 0 ? (
        <EmptyState title="No data" />
      ) : (
        <ul className="space-y-5">
          {envelopeRequests.map((request) => <EnvelopeRequestCard key={request.requestId} request={request} />)}
          {approvals.map((approval) => <ApprovalCard approval={approval} key={approval.approvalId} />)}
        </ul>
      )}</ResourceBoundary>
    </section>
  );
}

export function EnvelopeRequestCard({ request }: Readonly<{ request: BrowserEnvelopeRequestView }>) {
  const session = useSession();
  const [decision, setDecision] = useState<BrowserEnvelopeRequestDecisionResponse | null>(null);
  const [status, setStatus] = useState<EnvelopeActionState>("idle");
  const requested = request.requestedEnvelope.spec;
  const governing = request.templateEnvelope.spec;
  const terminal = decision?.request.status === "provisioned" || decision?.request.status === "rejected";

  async function approveRequest() {
    if (session.status !== "authenticated" || terminal) return;
    setStatus("approving");
    const result = await approveAdminEnvelopeRequest({
      body: {},
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
      setStatus("rejected");
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
          <DefinitionList items={[
            ["Models", requested.llms.map((model) => `${model.provider}/${model.model}`).join(", ") || "None"],
            ["Tools", requested.tools.map((tool) => `${tool.provider}/${tool.resource}:${tool.action}`).join(", ") || "None"],
            ["Budget", `${requested.budget.monthlyLimit} ${requested.budget.currency}`],
            ["TTL", requested.ttl],
          ]} />
        </section>
        <section aria-label="Governing template" className="space-y-3">
          <h3 className="font-semibold">Governing template</h3>
          <DefinitionList items={[
            ["Models", governing.llms.map((model) => `${model.provider}/${model.model}`).join(", ") || "None"],
            ["Tools", governing.tools.map((tool) => `${tool.provider}/${tool.resource}:${tool.action}`).join(", ") || "None"],
            ["Budget", `${governing.budget.monthlyLimit} ${governing.budget.currency}`],
            ["TTL", governing.ttl],
          ]} />
        </section>
      </div>
      <div className="flex flex-wrap gap-3 border-t pt-5">
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50" disabled={terminal || status === "approving" || status === "rejecting"} onClick={() => void approveRequest()} type="button">{status === "approving" ? "Approving…" : "Approve request"}</button>
      </div>
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
        <p className={status === "provisioned" || status === "rejected" ? "text-sm text-green-800" : "text-sm text-red-800"} role={status === "provisioned" || status === "rejected" ? "status" : "alert"}>
          {status === "provisioned" ? "The exact requested envelope was provisioned." : status === "rejected" ? "The envelope request was rejected without creating an envelope." : status === "conflict" ? "This request or its template revision is stale. Reload the authoritative queue." : status === "forbidden" ? "The Rust authorization boundary rejected this mutation." : status === "unavailable" ? "The envelope request authority is unavailable." : "The envelope request response could not be accepted."}
        </p>
      ) : null}
    </li>
  );
}

function ApprovalCard({ approval }: Readonly<{ approval: BrowserApprovalView }>) {
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
