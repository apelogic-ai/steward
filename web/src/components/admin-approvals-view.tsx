"use client";

import Link from "next/link";
import { useCallback, useState, type FormEvent, type ReactNode } from "react";

import {
  approveAdminApproval,
  approveAdminEnvelopeRequest,
  denyAdminEscalation,
  fileAdminApprovalDecision,
  fileAdminEnvelopeRequest,
  getAdminRequest,
  listAdminRequests,
  topUpAdminEscalation,
  rejectAdminEnvelopeRequest,
  type BrowserApprovalView,
  type BrowserDecisionReferenceResponse,
  type BrowserEnvelopeSpec,
  type BrowserEnvelopeRequestDecisionResponse,
  type BrowserEnvelopeRequestView,
  type AdminRequestView,
  type AdminRequestResponse,
  type AdminRequestsResponse,
} from "@/api-client";
import { DataTable, FilterTabs, GrantChipList, Meter, SectionCard, grantKindForAction } from "@/components/hs";
import { ConfirmationDialog } from "@/components/hs/confirmation-dialog";
import { DefinitionList, PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
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
  const [filter, setFilter] = useState("needs_action");
  const load = useCallback(() => listAdminRequests({ cache: "no-store", credentials: "same-origin", query: { state: "all", limit: 100 } }), []);
  const state = useApiResource<AdminRequestsResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Envelope requests within a template ceiling are approved and provisioned automatically. Requests above a ceiling, and runtimes that exhaust a cumulative limit, wait here for a decision." title="Requests" />
      <ResourceBoundary state={state}>{({ requests }) => {
        if (requests.length === 0) return <p className="rounded-card border bg-panel p-6 text-sm text-muted-ink">Nothing needs action.</p>;
        const counts = {
          needs_action: requests.filter((request) => request.state === "requested" || request.state === "escalated").length,
          all: requests.length,
          auto_approved: requests.filter((request) => request.state === "auto_approved").length,
          rejected: requests.filter((request) => request.state === "rejected").length,
        };
        const visible = filter === "all" ? requests : filter === "needs_action"
          ? requests.filter((request) => request.state === "requested" || request.state === "escalated")
          : requests.filter((request) => request.state === filter);
        return <div className="space-y-5"><FilterTabs active={filter} items={[
          { count: counts.needs_action, label: "Needs action", value: "needs_action" },
          { count: counts.all, label: "All", value: "all" },
          { count: counts.auto_approved, label: "Auto-approved", value: "auto_approved" },
          { count: counts.rejected, label: "Rejected", value: "rejected" },
        ]} onChange={setFilter} />{visible.length ? <RequestTable requests={visible} /> : <p className="rounded-card border bg-panel p-6 text-sm text-muted-ink">Nothing needs action.</p>}</div>;
      }}</ResourceBoundary>
    </section>
  );
}

function requestReason(request: AdminRequestView): string {
  return request.kind === "ceiling_exceeded" ? "above ceiling"
    : request.kind === "cumulative_exhausted" ? "cumulative limit"
      : request.kind === "within_ceiling" ? "within ceiling" : "custom";
}

function triggerLabel(request: AdminRequestView): [string, string] {
  if (request.kind === "ceiling_exceeded") return ["Above ceiling", request.deltas.map((delta) => `${delta.dimension} ${deltaValue(delta.requested)} > ${deltaValue(delta.ceiling)}`).join(" · ")];
  if (request.kind === "cumulative_exhausted") {
    const meter = request.escalation?.meters[0];
    return ["Cumulative limit reached", meter ? `${meter.used} / ${meter.limit} ${meter.unit}` : "Limit exhausted"];
  }
  if (request.kind === "within_ceiling") return ["Within ceiling", "Fits the template ceiling"];
  return ["Custom request", "No governing template"];
}

function requestAge(createdAt: string): string {
  const seconds = Math.max(0, Math.round((Date.now() - new Date(createdAt).valueOf()) / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.round(minutes / 60);
  return hours < 24 ? `${hours}h` : `${Math.round(hours / 24)}d`;
}

function RequestTable({ requests }: Readonly<{ requests: Array<AdminRequestView> }>) {
  return <DataTable ariaLabel="Requests" columns={[
    { key: "request", label: "Request", className: "font-semibold", render: (request) => <span><span className="block truncate">{request.template.displayName ?? "Custom envelope"} · {requestReason(request)}</span><span className="mt-0.5 block truncate font-mono text-xs font-normal text-muted-ink">{request.id}</span></span> },
    { key: "requester", label: "Requester", className: "truncate", render: (request) => request.requester.displayEmail },
    { key: "trigger", label: "Trigger", render: (request) => { const [label, detail] = triggerLabel(request); return <span><span className={`block font-medium ${request.kind === "ceiling_exceeded" ? "text-warn" : request.kind === "cumulative_exhausted" ? "text-err" : "text-muted-ink"}`}>{label}</span><span className="mt-0.5 block truncate font-mono text-xs text-muted-ink">{detail}</span></span>; } },
    { key: "status", label: "Status", render: (request) => <StatusBadge value={request.state} /> },
    { key: "age", label: "Age", className: "text-right font-mono text-muted-ink", render: (request) => requestAge(request.createdAt) },
    { key: "open", label: "", className: "text-right text-lg text-faint-ink", render: () => "›" },
  ]} gridTemplateColumns="minmax(230px,1.2fr) minmax(180px,.85fr) minmax(220px,1fr) 120px 70px 24px" minWidth="980px" rowHref={(request) => `/admin/approvals/${encodeURIComponent(request.id)}`} rowKey={(request) => request.id} rows={requests} />;
}

export function AdminRequestDetailView({ requestId }: Readonly<{ requestId: string }>) {
  const load = useCallback(() => getAdminRequest({ cache: "no-store", credentials: "same-origin", path: { request_id: requestId } }), [requestId]);
  const state = useApiResource<AdminRequestResponse>(load);
  return <ResourceBoundary state={state}>{({ request }) => <section aria-labelledby="page-title" className="mx-auto max-w-[980px] space-y-5"><PageHeader description={`${request.requester.displayEmail} · requested ${new Date(request.createdAt).toLocaleString()}`} title={<span className="inline-flex flex-wrap items-center gap-3">{request.template.displayName ?? "Custom envelope"} · {requestReason(request)} <StatusBadge value={request.state} /></span>} />{request.kind === "ceiling_exceeded" ? <div className="rounded-[10px] bg-warn-soft px-[18px] py-3.5"><strong className="block text-[13px] font-semibold text-warn">Requested above the template ceiling</strong><p className="mt-1 font-mono text-sm">{triggerLabel(request)[1]}</p></div> : request.kind === "cumulative_exhausted" ? <div className="rounded-[10px] bg-err-soft px-[18px] py-3.5"><strong className="block text-[13px] font-semibold text-err">Escalated: a cumulative limit was reached and the run is parked</strong><p className="mt-1 font-mono text-sm">{triggerLabel(request)[1]}</p></div> : null}<ul><UnifiedRequestCard request={request} /></ul><Link className="inline-flex text-sm font-semibold" href="/admin/approvals">← Back to requests</Link></section>}</ResourceBoundary>;
}

function deltaValue(value: unknown): string {
  if (value === null || value === undefined) return "Not set";
  if (typeof value === "string" || typeof value === "number") return String(value);
  return JSON.stringify(value);
}

function deltaLabel(dimension: AdminRequestView["deltas"][number]["dimension"]): string {
  return {
    budget: "Monthly limit",
    singleRunBudget: "Per-run limit",
    runtimeMinutes: "Runtime minutes",
    ttl: "TTL",
    models: "Models",
    tools: "Tools",
    runnerPlatforms: "Platforms",
    runnerMemory: "Memory",
    runnerCompute: "Compute",
    runnerStorage: "Storage",
  }[dimension];
}

function deltaSide(delta: AdminRequestView["deltas"][number], side: "ceiling" | "requested"): ReactNode {
  if (delta.dimension === "models") {
    return <GrantChipList grants={delta[side].map((model) => ({ kind: "model", name: `${model.provider}/${model.model}` }))} />;
  }
  if (delta.dimension === "tools") {
    return <GrantChipList grants={delta[side].map((tool) => ({ kind: grantKindForAction(tool.action), name: `${tool.resource}:${tool.action}` }))} />;
  }
  const value = delta[side];
  const suffix = "currency" in delta ? ` ${delta.currency}` : "";
  return <span className="font-mono text-sm">{deltaValue(value)}{value === null || value === undefined ? "" : suffix}</span>;
}

function numericDelta(delta: AdminRequestView["deltas"][number]): string | null {
  if (!(delta.dimension === "budget" || delta.dimension === "singleRunBudget" || delta.dimension === "runtimeMinutes")) return null;
  if (delta.requested === null || delta.requested === undefined) return null;
  const difference = Number(delta.requested) - Number(delta.ceiling);
  return Number.isFinite(difference) && difference > 0 ? `+${difference.toFixed(2).replace(/\.00$/, "")}` : null;
}

function RequestComparison({ request }: Readonly<{ request: AdminRequestView }>) {
  if (request.deltas.length === 0) return <p className="rounded-control border border-line bg-notice p-4 text-sm">{request.kind === "custom" ? "This custom request has no governing template." : "This request is within the configured ceiling."}</p>;
  return (
    <SectionCard title="Requested vs template ceiling">
      <div className="overflow-x-auto">
        <div className="min-w-[520px]">
          <div className="grid grid-cols-[minmax(90px,130px)_1fr_1fr] gap-4 border-b border-line-soft pb-2 text-xs font-medium text-muted-ink"><span>Field</span><span>Requested</span><span>{request.template.id ? `${request.template.id} rev ${request.template.revision ?? "—"}` : "Ceiling"}</span></div>
          {request.deltas.map((delta, index) => {
            const over = numericDelta(delta);
            return <div className="grid grid-cols-[minmax(90px,130px)_1fr_1fr] gap-4 border-b border-line-soft py-3 last:border-0" key={`${delta.dimension}-${index}`}><strong className="text-sm">{deltaLabel(delta.dimension)}</strong><div className={over ? "font-semibold text-err" : ""}>{deltaSide(delta, "requested")}{over ? <span className="ms-2 rounded-full bg-err-soft px-2 py-0.5 text-xs font-semibold text-err">{over}</span> : null}</div><div className="opacity-80">{deltaSide(delta, "ceiling")}</div></div>;
          })}
        </div>
      </div>
    </SectionCard>
  );
}

function RequestHistory({ request }: Readonly<{ request: AdminRequestView }>) {
  return <SectionCard title="History"><ol className="space-y-4">{request.history.map((event, index) => <li className="relative ps-5" key={`${event.at}-${index}`}><span aria-hidden="true" className={`absolute start-0 top-1 size-2.5 rounded-full border-2 ${event.state === "rejected" || event.state === "expired" ? "border-err" : event.state === "approved" || event.state === "auto_approved" ? "border-ok" : "border-faint-ink"}`} /><p className="text-sm font-semibold">{event.state.replaceAll("_", " ")}</p><p className="mt-0.5 text-[13px] text-muted-ink">by {event.actor} · {new Date(event.at).toLocaleString()}{event.reason ? ` · “${event.reason}”` : ""}</p></li>)}</ol></SectionCard>;
}

export function UnifiedRequestCard({ request }: Readonly<{ request: AdminRequestView }>) {
  const session = useSession();
  const [reference, setReference] = useState<{ decisionKey: string; evidenceUrl: string } | null>(request.decision?.decisionKey && request.decision.evidenceUrl ? {
    decisionKey: request.decision.decisionKey,
    evidenceUrl: request.decision.evidenceUrl,
  } : null);
  const [status, setStatus] = useState<UnifiedRequestActionState>("idle");
  const [denial, setDenial] = useState<string | null>(null);
  const [rejection, setRejection] = useState<string | null>(null);
  const terminal = request.state === "approved" || request.state === "auto_approved" || request.state === "rejected" || request.state === "expired";

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

  async function rejectEnvelope(reason: string) {
    if (session.status !== "authenticated" || request.source !== "envelope_request") return;
    setRejection(null);
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

  async function deny(rationale: string) {
    if (session.status !== "authenticated") return;
    setDenial(null);
    setStatus("approving");
    const result = await denyAdminEscalation({
      body: { rationale },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
      path: { escalation_id: request.id },
    });
    setStatus(result.data && result.response?.ok ? "rejected" : classifyMutationFailure(result.response?.status));
  }

  if (request.source === "escalation" && request.escalation) {
    return (
      <li className="space-y-5">
        <SectionCard title="Cumulative usage this period">
          <ul className="space-y-5">{request.escalation.meters.map((meter) => <li key={meter.dimension}><Meter label={meter.dimension === "llm_spend" ? "LLM spend" : "Runtime minutes"} limit={Number(meter.limit)} limitLabel={meter.limit} unit={meter.unit} used={Number(meter.used)} usedLabel={meter.used} /><p className="mt-1 text-xs text-muted-ink">Observed {new Date(meter.observedAt).toLocaleString()}</p></li>)}</ul>
          <div className="mt-5 flex flex-wrap items-center justify-between gap-3 border-t border-line-soft pt-4 text-sm text-muted-ink"><span>Blocked run <span className="font-mono text-ink">{request.escalation.blockedTaskUid}</span> · parked since {new Date(request.escalation.parkedAt).toLocaleString()}</span><Link className="font-semibold text-ink" href={`/admin/runs/${encodeURIComponent(request.escalation.blockedTaskUid)}`}>Open run →</Link></div>
        </SectionCard>
        {terminal ? <RequestHistory request={request} /> : <SectionCard title="Decision"><form className="grid gap-3 sm:grid-cols-2" onSubmit={topUp}>
          <label className="grid gap-2 text-sm font-semibold">Top-up amount ({request.escalation.meters[0]?.unit ?? "units"})<input className="min-h-11 rounded-md border px-3 font-normal" inputMode="decimal" name="amount" required /></label>
          <label className="grid gap-2 text-sm font-semibold">Valid until (RFC 3339)<input className="min-h-11 rounded-md border px-3 font-normal" name="validUntil" placeholder="2026-08-25T17:00:00Z" required /></label>
          <label className="grid gap-2 text-sm font-semibold sm:col-span-2">Rationale<textarea className="min-h-20 rounded-md border p-3 font-normal" name="rationale" placeholder="Why is additional authority appropriate?" required /></label>
          <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50 sm:justify-self-end" disabled={status === "approving" || status === "approved" || status === "rejected"} type="submit">Approve top-up and resume</button>
        </form>
        <form className="mt-4 grid gap-3 border-t border-line-soft pt-4" onSubmit={(event) => { event.preventDefault(); const fields = new FormData(event.currentTarget); setDenial(String(fields.get("denyRationale") ?? "").trim()); }}><label className="grid gap-2 text-sm font-semibold">Denial rationale<textarea className="min-h-20 rounded-md border p-3 font-normal" name="denyRationale" required /></label><button className="min-h-11 rounded-md border border-danger-line px-4 py-2 text-sm font-semibold text-err disabled:opacity-50 sm:justify-self-start" disabled={status === "approving" || status === "approved" || status === "rejected"} type="submit">Deny and cancel run</button></form>
        <ConfirmationDialog cancelLabel="Keep pending" confirmLabel="Deny and cancel run" description="The parked run will be cancelled and finalized. This action cannot be undone." onConfirm={() => void deny(denial ?? "")} onOpenChange={(open) => { if (!open) setDenial(null); }} open={denial !== null} pending={status === "approving"} title="Deny and cancel this run?" /></SectionCard>}
      </li>
    );
  }

  return (
    <li className="space-y-5">
      <RequestComparison request={request} />
      {terminal ? <RequestHistory request={request} /> : <SectionCard title="Decision"><div className="grid gap-5"><div className="grid grid-cols-[24px_minmax(0,1fr)] gap-3"><span className="grid size-6 place-items-center rounded-full bg-line-soft font-mono text-xs font-semibold">1</span><div><h3 className="text-sm font-semibold">File a decision reference</h3><p className="mt-1 text-[13px] text-muted-ink">Creates the governed record that serves as evidence for the exception.</p>{reference ? <div className="mt-3"><DefinitionList items={[["Decision key", reference.decisionKey], ["Evidence URL", reference.evidenceUrl]]} /></div> : <button className="mt-3 min-h-10 rounded-control border px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={status === "filing"} onClick={() => void fileDecision()} type="button">{status === "filing" ? "Filing…" : "File reference"}</button>}</div></div><div className={`grid grid-cols-[24px_minmax(0,1fr)] gap-3 ${reference ? "" : "opacity-55"}`}><span className="grid size-6 place-items-center rounded-full bg-line-soft font-mono text-xs font-semibold">2</span><div><h3 className="text-sm font-semibold">Approve or reject</h3><form className="mt-3 grid gap-4 sm:grid-cols-2" onSubmit={approve}>
        <label className="grid gap-2 text-sm font-semibold sm:col-span-2">Rationale<textarea className="min-h-24 rounded-md border p-3 font-normal" name="rationale" required /></label>
        <label className="grid gap-2 text-sm font-semibold">Expires at{request.source === "envelope_request" ? " (optional)" : " (RFC 3339)"}<input className="min-h-11 rounded-md border px-3 font-normal" name="expiresAt" placeholder="2026-08-25T17:00:00Z" required={request.source !== "envelope_request"} /></label>
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50 sm:self-end" disabled={!reference || status === "approving" || status === "rejecting" || status === "approved" || status === "rejection-complete"} type="submit">{status === "approving" ? "Approving…" : status === "approved" ? "Approved" : "Approve exception"}</button>
      </form>
      {request.source === "envelope_request" ? <form className="grid gap-3" onSubmit={(event) => { event.preventDefault(); const fields = new FormData(event.currentTarget); setRejection(String(fields.get("reason") ?? "").trim()); }}>
        <label className="grid gap-2 text-sm font-semibold">Rejection reason (optional)<textarea className="min-h-20 rounded-md border p-3 font-normal" maxLength={2000} name="reason" /></label>
        <button className="min-h-11 rounded-md border border-danger-line px-4 py-2 text-sm font-semibold text-err disabled:opacity-50 sm:justify-self-start" disabled={!reference || status === "approving" || status === "rejecting" || status === "approved" || status === "rejection-complete"} type="submit">{status === "rejecting" ? "Rejecting…" : "Reject"}</button>
      </form> : null}</div></div></div>
      <ConfirmationDialog cancelLabel="Keep pending" confirmLabel="Reject request" description="The requested authority will not be provisioned. The decision and optional reason will remain in the request history." onConfirm={() => void rejectEnvelope(rejection ?? "")} onOpenChange={(open) => { if (!open) setRejection(null); }} open={rejection !== null} pending={status === "rejecting"} title="Reject this request?" /></SectionCard>}
      {status !== "idle" && status !== "filing" && status !== "approving" && status !== "rejecting" ? <p className={status === "approved" || status === "filed" || status === "rejection-complete" ? "text-sm text-green-800" : "text-sm text-red-800"} role={status === "approved" || status === "filed" || status === "rejection-complete" ? "status" : "alert"}>{actionMessage(status)}</p> : null}
    </li>
  );
}

export function EnvelopeRequestCard({ request }: Readonly<{ request: BrowserEnvelopeRequestView }>) {
  const session = useSession();
  const [decision, setDecision] = useState<BrowserEnvelopeRequestDecisionResponse | null>(null);
  const [status, setStatus] = useState<EnvelopeActionState>("idle");
  const requested = request.requestedEnvelope.spec;
  const governing = request.templateEnvelope?.spec;
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
        ["Template", request.templateId ?? "Custom"],
        ["Template revision", request.templateRevision?.toString() ?? "Not applicable"],
        ["Created", request.createdAt],
      ]} />
      <div className="grid gap-5 border-t pt-5 lg:grid-cols-2">
        <section aria-label="Requested authority" className="space-y-3">
          <h3 className="font-semibold">Requested authority</h3>
          <DefinitionList items={envelopeAuthorityItems(requested)} />
        </section>
        {governing ? <section aria-label="Governing template" className="space-y-3">
          <h3 className="font-semibold">Governing template</h3>
          <DefinitionList items={envelopeAuthorityItems(governing)} />
        </section> : <section aria-label="Governing template" className="space-y-3">
          <h3 className="font-semibold">Governing template</h3>
          <p className="text-sm text-muted-ink">Custom requests require administrator review and are not auto-provisioned.</p>
        </section>}
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
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50 sm:justify-self-start" disabled={terminal || status === "approving" || status === "rejecting"} type="submit">{status === "approving" ? "Approving…" : "Approve request"}</button>
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
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50 sm:col-span-2 sm:justify-self-start" disabled={!reference || status === "approving" || status === "approved"} type="submit">{status === "approving" ? "Approving…" : status === "approved" ? "Approved" : "Approve exception"}</button>
      </form>
      {status !== "idle" && status !== "filing" && status !== "approving" ? (
        <p className={status === "approved" || status === "filed" ? "text-sm text-green-800" : "text-sm text-red-800"} role={status === "approved" || status === "filed" ? "status" : "alert"}>{actionMessage(status)}</p>
      ) : null}
    </li>
  );
}
