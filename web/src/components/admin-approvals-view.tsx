"use client";

import { useCallback, useState, type FormEvent } from "react";

import {
  approveAdminApproval,
  fileAdminApprovalDecision,
  listAdminApprovals,
  type BrowserApprovalsResponse,
  type BrowserApprovalView,
  type BrowserDecisionReferenceResponse,
} from "@/api-client";
import { DefinitionList, EmptyState, PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { classifyMutationFailure } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

type ApprovalActionState = "idle" | "filing" | "filed" | "approving" | "approved" | "conflict" | "rejected" | "forbidden" | "unavailable" | "error";

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
  const load = useCallback(() => listAdminApprovals({ cache: "no-store", credentials: "same-origin" }), []);
  const state = useApiResource<BrowserApprovalsResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Review parked envelope exceptions and apply only server-recorded decisions." eyebrow="Administration" title="Pending approvals" />
      <ResourceBoundary state={state}>{({ approvals }) => approvals.length === 0 ? (
        <EmptyState title="No pending approvals"><p>The authoritative approval queue is empty.</p></EmptyState>
      ) : (
        <ul className="space-y-5">{approvals.map((approval) => <ApprovalCard approval={approval} key={approval.approvalId} />)}</ul>
      )}</ResourceBoundary>
    </section>
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
