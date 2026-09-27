"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useRef, useState, type FormEvent } from "react";

import {
  createRequest,
  getRequest,
  listRequests,
  listTemplates,
  myRuns,
  type AvailableEnvelopeTemplate,
  type BrowserEnvelope,
  type EnvelopeRequestResponse,
  type EnvelopeRequestsResponse,
  type EnvelopeTemplatesResponse,
  type GithubActionsWorkflowResponse,
  type MyRunsResponse,
  type UserEnvelopeRequest,
} from "@/api-client";
import { RunCards } from "@/components/run-views";
import { CodeBlock, DataTable, FilterTabs, FormSection, GrantChipList, Meter, SectionCard, StatStrip, TagSelect, grantKindForAction } from "@/components/hs";
import { EmptyState, PageHeader, PrimaryLink, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { classifyMutationFailure, type MutationFailureState } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";
import { listPublishedWorkflows, renderWorkflowForEnvelope, type PublishedWorkflowListResponse } from "@/workflows/api";
import { workflowReference } from "@/workflows/contracts";

function dateTime(value: string): string {
  const parsed = new Date(value);
  return Number.isNaN(parsed.valueOf()) ? value : parsed.toLocaleString();
}

export function EnvelopesView() {
  const [status, setStatus] = useState("all");
  const load = useCallback(() => listRequests({ cache: "no-store", credentials: "same-origin" }), []);
  const state = useApiResource<EnvelopeRequestsResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader actions={<PrimaryLink href="/envelopes/new">Request envelope</PrimaryLink>} description="Request and inspect governed runtime authority." title="Envelopes" />
      <ResourceBoundary state={state}>{({ requests }) => {
        if (requests.length === 0) return <EmptyState title="No data" />;
        const counts = new Map<string, number>();
        for (const request of requests) counts.set(request.status, (counts.get(request.status) ?? 0) + 1);
        const filtered = status === "all" ? requests : requests.filter((request) => request.status === status);
        return <div className="space-y-5">
          <FilterTabs active={status} items={[
            { count: requests.length, label: "All", value: "all" },
            ...Array.from(counts).map(([value, count]) => ({ count, label: value.charAt(0).toUpperCase() + value.slice(1), value })),
          ]} onChange={setStatus} />
          <EnvelopeTable requests={filtered} />
        </div>;
      }}</ResourceBoundary>
    </section>
  );
}

function EnvelopeTable({ requests }: Readonly<{ requests: Array<UserEnvelopeRequest> }>) {
  if (requests.length === 0) return <EmptyState title="No matching envelopes" />;
  return <DataTable
    ariaLabel="Envelopes"
    columns={[
      { key: "name", label: "Envelope", className: "font-semibold", render: (request) => <span><span className="block truncate">{request.templateId ?? "Custom envelope"}</span><span className="mt-0.5 block truncate font-mono text-xs font-normal text-muted-ink">{request.id}</span></span> },
      { key: "status", label: "Status", render: (request) => <StatusBadge value={request.status} /> },
      { key: "revision", label: "Revision", className: "tabular-nums text-muted-ink", render: (request) => request.approvedEnvelope?.revision ?? request.requestedEnvelope.revision },
      { key: "usage", label: "Monthly spend", render: (request) => {
        const spend = request.usage?.spend;
        if (!spend) {
          const envelope = request.approvedEnvelope ?? request.requestedEnvelope;
          return <span className="text-muted-ink">{request.usage?.availability.reason ?? "Not reported"} · limit <strong className="font-medium text-ink">{envelope.spec.budget.monthlyLimit} {envelope.spec.budget.currency}</strong></span>;
        }
        return <Meter label={spend.currency} limit={Number(spend.limit)} limitLabel={spend.limit} used={Number(spend.observed)} usedLabel={spend.observed} />;
      } },
      { key: "updated", label: "Updated", className: "text-muted-ink", render: (request) => dateTime(request.statusAt) },
    ]}
    gridTemplateColumns="minmax(220px,1.5fr) 130px 90px minmax(190px,1fr) 180px"
    minWidth="850px"
    rowHref={(request) => `/envelopes/${request.id}`}
    rowKey={(request) => request.id}
    rows={requests}
  />;
}

export function NewEnvelopeView() {
  const load = useCallback(() => listTemplates({ cache: "no-store", credentials: "same-origin" }), []);
  const state = useApiResource<EnvelopeTemplatesResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Pick a template, then narrow it. You can’t go above the template’s ceiling." title="Request an envelope" />
      <ResourceBoundary state={state}>{({ templates }) => <EnvelopeRequestForm templates={templates} />}</ResourceBoundary>
    </section>
  );
}

function EnvelopeRequestForm({ templates }: Readonly<{ templates: Array<AvailableEnvelopeTemplate> }>) {
  const [requestType, setRequestType] = useState<"template" | "custom">(templates.length ? "template" : "custom");
  const [submitting, setSubmitting] = useState(false);
  return (
    <div className="space-y-5">
      <label className="grid max-w-md gap-2 text-sm font-semibold">Request type
        <select className="min-h-11 rounded-md border bg-panel px-3 font-normal" disabled={submitting} onChange={(event) => setRequestType(event.target.value === "custom" ? "custom" : "template")} value={requestType}>
          {templates.length ? <option value="template">Use a template</option> : null}
          <option value="custom">Custom envelope</option>
        </select>
      </label>
      {requestType === "template" && templates.length
        ? <TemplateEnvelopeRequestForm onSubmittingChange={setSubmitting} templates={templates} />
        : <CustomEnvelopeRequestForm onSubmittingChange={setSubmitting} />}
    </div>
  );
}

function TemplateEnvelopeRequestForm({ onSubmittingChange, templates }: Readonly<{ onSubmittingChange: (submitting: boolean) => void; templates: Array<AvailableEnvelopeTemplate> }>) {
  const router = useRouter();
  const session = useSession();
  const [templateId, setTemplateId] = useState(templates[0]?.id ?? "");
  const template = templates.find((item) => item.id === templateId) ?? templates[0];
  const [budget, setBudget] = useState(template.ceiling.spec.budget.monthlyLimit);
  const [runtimeMinutes, setRuntimeMinutes] = useState(template.ceiling.spec.runtimeMinutesLimit ?? "");
  const [ttl, setTtl] = useState(template.ceiling.spec.ttl);
  const [models, setModels] = useState(() => new Set(template.ceiling.spec.llms.map((item) => `${item.provider}\u0000${item.model}`)));
  const [tools, setTools] = useState(() => new Set(template.ceiling.spec.tools.map((item) => `${item.provider}\u0000${item.resource}\u0000${item.action}`)));
  const [submission, setSubmission] = useState<"idle" | "submitting" | MutationFailureState>("idle");

  function selectTemplate(id: string) {
    const next = templates.find((item) => item.id === id);
    if (!next) return;
    setTemplateId(id);
    setBudget(next.ceiling.spec.budget.monthlyLimit);
    setRuntimeMinutes(next.ceiling.spec.runtimeMinutesLimit ?? "");
    setTtl(next.ceiling.spec.ttl);
    setModels(new Set(next.ceiling.spec.llms.map((item) => `${item.provider}\u0000${item.model}`)));
    setTools(new Set(next.ceiling.spec.tools.map((item) => `${item.provider}\u0000${item.resource}\u0000${item.action}`)));
    setSubmission("idle");
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated") return;
    setSubmission("submitting");
    onSubmittingChange(true);
    const result = await createRequest({
      body: {
        idempotencyKey: crypto.randomUUID(),
        templateId: template.id,
        templateRevision: template.revision,
        requestedEnvelope: {
          revision: template.revision,
          spec: {
            ...template.ceiling.spec,
            budget: {
              currency: template.ceiling.spec.budget.currency,
              monthlyLimit: budget,
              singleRunLimit: template.ceiling.spec.budget.singleRunLimit,
            },
            ...(runtimeMinutes.trim() ? { runtimeMinutesLimit: runtimeMinutes.trim() } : {}),
            ttl,
            llms: template.ceiling.spec.llms.filter((item) => models.has(`${item.provider}\u0000${item.model}`)),
            tools: template.ceiling.spec.tools.filter((item) => tools.has(`${item.provider}\u0000${item.resource}\u0000${item.action}`)),
          },
        },
      },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
    });
    if (result.data && result.response?.status === 201) {
      router.push(`/envelopes/${result.data.request.id}`);
      return;
    }
    setSubmission(classifyMutationFailure(result.response?.status));
    onSubmittingChange(false);
  }

  const modelOptions = template.ceiling.spec.llms.map((item) => ({ key: `${item.provider}\u0000${item.model}`, kind: "model" as const, label: `${item.provider}/${item.model}` }));
  const toolOptions = template.ceiling.spec.tools.map((item) => ({ key: `${item.provider}\u0000${item.resource}\u0000${item.action}`, kind: grantKindForAction(item.action), label: `${item.provider}:${item.resource}:${item.action}` }));
  return (
    <form className="flex flex-wrap items-start gap-5" onSubmit={submit}>
      <SectionCard className="min-w-0 flex-[1_1_520px]">
        <FormSection number="01" title="Template"><label className="grid gap-2 text-sm font-semibold">Template<select className="min-h-11 rounded-control border bg-panel px-3 font-normal" disabled={submission === "submitting"} onChange={(event) => selectTemplate(event.target.value)} value={template.id}>{templates.map((item) => <option key={item.id} value={item.id}>{item.displayName} · revision {item.revision}</option>)}</select></label></FormSection>
        <FormSection number="02" title="Budget and lifetime"><div className="grid gap-4 sm:grid-cols-3"><label className="grid gap-2 text-sm font-semibold">Monthly limit ({template.ceiling.spec.budget.currency})<input className="min-h-11 rounded-control border px-3 font-normal" disabled={submission === "submitting"} inputMode="decimal" onChange={(event) => setBudget(event.target.value)} required value={budget} /><span className="text-xs font-normal text-muted-ink">Ceiling {template.ceiling.spec.budget.monthlyLimit}</span></label><label className="grid gap-2 text-sm font-semibold">Time to live<input className="min-h-11 rounded-control border px-3 font-normal" disabled={submission === "submitting"} onChange={(event) => setTtl(event.target.value)} required value={ttl} /><span className="text-xs font-normal text-muted-ink">Ceiling {template.ceiling.spec.ttl}</span></label>{template.ceiling.spec.runtimeMinutesLimit ? <label className="grid gap-2 text-sm font-semibold">Runtime minutes / month<input className="min-h-11 rounded-control border px-3 font-normal" disabled={submission === "submitting"} inputMode="decimal" onChange={(event) => setRuntimeMinutes(event.target.value)} required value={runtimeMinutes} /><span className="text-xs font-normal text-muted-ink">Ceiling {template.ceiling.spec.runtimeMinutesLimit}</span></label> : null}</div></FormSection>
        <FormSection number="03" title="Models"><TagSelect disabled={submission === "submitting"} label="Models" onChange={(next) => setModels(new Set(next))} options={modelOptions} value={[...models]} /></FormSection>
        <FormSection description="Remove anything this envelope doesn’t need. Only tools in the template ceiling are offered." number="04" title="Tools"><TagSelect disabled={submission === "submitting"} label="Tools" onChange={(next) => setTools(new Set(next))} options={toolOptions} value={[...tools]} /></FormSection>
      </SectionCard>
      <SectionCard className="sticky top-7 flex-[1_1_280px] lg:max-w-[380px]" title="Summary">
        <dl className="space-y-3 text-sm">{[["Template", template.displayName], ["Monthly limit", `${budget} ${template.ceiling.spec.budget.currency}`], ["Per run", template.ceiling.spec.budget.singleRunLimit ?? "Not set"], ["TTL", ttl], ["Models", String(models.size)], ["Tools", String(tools.size)]].map(([label, value]) => <div className="flex justify-between gap-4" key={label}><dt className="text-muted-ink">{label}</dt><dd className="text-right font-medium">{value}</dd></div>)}</dl>
        {submission !== "idle" && submission !== "submitting" ? <p className="mt-4 text-sm text-err" role="alert">{{ conflict: "The template revision changed. Reload before retrying.", rejected: "Rust admission rejected the requested authority as outside the template ceiling.", forbidden: "The Rust authorization boundary rejected the request.", unavailable: "The authoritative request service is unavailable.", error: "The request could not be accepted." }[submission]}</p> : null}
        <button className="mt-5 min-h-10 w-full rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:cursor-not-allowed disabled:opacity-50" disabled={submission === "submitting"} type="submit">{submission === "submitting" ? "Submitting…" : "Submit request"}</button>
        <p className="mt-3 text-xs leading-5 text-muted-ink">{template.autoProvisionThreshold === null || template.autoProvisionThreshold === undefined ? "Provisioned instantly when the request stays within this template ceiling." : "Provisioned instantly when the request stays within the template’s auto-approve threshold; otherwise an administrator reviews it."}</p>
      </SectionCard>
    </form>
  );
}

function parseCustomEnvelope(value: string): BrowserEnvelope | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(value);
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return null;
  const envelope = parsed as Record<string, unknown>;
  if (!Number.isSafeInteger(envelope.revision) || typeof envelope.spec !== "object" || envelope.spec === null || Array.isArray(envelope.spec)) return null;
  const spec = envelope.spec as Record<string, unknown>;
  if (!Array.isArray(spec.llms) || !Array.isArray(spec.tools) || typeof spec.ttl !== "string") return null;
  if (typeof spec.budget !== "object" || spec.budget === null || Array.isArray(spec.budget)) return null;
  const budget = spec.budget as Record<string, unknown>;
  if (typeof budget.currency !== "string" || typeof budget.monthlyLimit !== "string") return null;
  return parsed as BrowserEnvelope;
}

function CustomEnvelopeRequestForm({ onSubmittingChange }: Readonly<{ onSubmittingChange: (submitting: boolean) => void }>) {
  const router = useRouter();
  const session = useSession();
  const [envelopeJson, setEnvelopeJson] = useState("");
  const [submission, setSubmission] = useState<"idle" | "submitting" | "invalid" | MutationFailureState>("idle");
  const idempotencyKey = useRef<string | null>(null);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated") return;
    const requestedEnvelope = parseCustomEnvelope(envelopeJson);
    if (!requestedEnvelope) {
      setSubmission("invalid");
      return;
    }
    setSubmission("submitting");
    onSubmittingChange(true);
    const result = await createRequest({
      body: {
        idempotencyKey: idempotencyKey.current ??= crypto.randomUUID(),
        requestedEnvelope,
      },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
    });
    if (result.data && result.response?.status === 201) {
      router.push(`/envelopes/${result.data.request.id}`);
      return;
    }
    setSubmission(classifyMutationFailure(result.response?.status));
    onSubmittingChange(false);
  }

  const failure = submission === "invalid"
    ? "Enter a complete envelope JSON object with revision, budget, models, tools, and TTL."
    : submission !== "idle" && submission !== "submitting"
      ? {
          conflict: "An equivalent envelope request already exists. Reload before retrying.",
          rejected: "Steward rejected the custom authority. It must fit the deployment safety ceiling and capability catalog.",
          forbidden: "The Rust authorization boundary rejected the request.",
          unavailable: "The authoritative request service is unavailable.",
          error: "The request could not be accepted.",
        }[submission]
      : null;

  return (
    <form className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm" onSubmit={submit}>
      <div>
        <h2 className="font-semibold">Complete requested authority</h2>
        <p className="mt-1 text-sm text-muted-ink">Custom requests have no governing template and always require administrator approval. Steward validates the complete envelope against the deployment safety ceiling; model, tool, and limit values are specific to this deployment.</p>
      </div>
      <label className="grid gap-2 text-sm font-semibold">Complete envelope JSON
        <textarea
          className="min-h-80 rounded-md border bg-canvas p-4 font-mono text-xs font-normal"
          disabled={submission === "submitting"}
          onChange={(event) => { setEnvelopeJson(event.target.value); idempotencyKey.current = null; setSubmission("idle"); }}
          placeholder={'{\n  "revision": 1,\n  "spec": {\n    "llms": [],\n    "tools": [],\n    "budget": { "monthlyLimit": "1.00", "currency": "USD" },\n    "runtimeMinutesLimit": "60",\n    "ttl": "15m",\n    "runner": {}\n  }\n}'}
          required
          spellCheck={false}
          value={envelopeJson}
        />
      </label>
      {failure ? <p className="text-sm text-red-800" role="alert">{failure}</p> : null}
      <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:cursor-not-allowed disabled:opacity-50" disabled={submission === "submitting"} type="submit">{submission === "submitting" ? "Submitting…" : "Submit request"}</button>
    </form>
  );
}

export function EnvelopeDetailView({ requestId }: Readonly<{ requestId: string }>) {
  const load = useCallback(() => getRequest({ cache: "no-store", credentials: "same-origin", path: { request_id: requestId } }), [requestId]);
  const state = useApiResource<EnvelopeRequestResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <ResourceBoundary state={state}>{({ request }) => <EnvelopeDetail request={request} />}</ResourceBoundary>
    </section>
  );
}

function EnvelopeDetail({ request }: Readonly<{ request: UserEnvelopeRequest }>) {
  const envelope = request.approvedEnvelope ?? request.requestedEnvelope;
  const usage = request.usage?.spend;
  return (
    <div className="space-y-6">
      <PageHeader
        actions={request.envelopeInstanceId ? <Link className="inline-flex h-10 items-center rounded-control border bg-panel px-4 text-sm font-semibold hover:bg-subtle" href={`/envelopes/${request.id}/runs`}>View runs</Link> : null}
        description={request.id}
        title={<span className="flex flex-wrap items-center gap-3">{request.templateId ?? "Custom envelope"}<span aria-hidden="true"><StatusBadge value={request.status} /></span></span>}
      />
      <StatStrip items={[
        {
          label: "Spend this month",
          value: usage ? `${usage.observed} ${usage.currency}` : "Not reported",
          detail: usage ? <Meter limit={Number(usage.limit)} limitLabel={usage.limit} unit={usage.currency} used={Number(usage.observed)} usedLabel={usage.observed} /> : request.usage?.availability.reason,
        },
        { label: "Per-run limit", value: envelope.spec.budget.singleRunLimit ? `${envelope.spec.budget.singleRunLimit} ${envelope.spec.budget.currency}` : "Not set" },
        { label: "Time to live", value: <span className="font-mono">{envelope.spec.ttl}</span> },
        { label: "Envelope instance", value: <span className="font-mono text-sm">{request.envelopeInstanceId ?? "Not assigned"}</span> },
      ]} />
      {request.reason ? <p className="rounded-card bg-notice p-4 text-sm"><strong>Server reason:</strong> {request.reason}</p> : null}
      <div className="grid gap-5 lg:grid-cols-2">
        <SectionCard title="Authority">
          <div className="space-y-5">
            <div><p className="mb-2 text-xs font-semibold text-muted-ink">Models</p><GrantChipList grants={envelope.spec.llms.map((model) => ({ kind: "model", name: `${model.provider}/${model.model}` }))} /></div>
            <div><p className="mb-2 text-xs font-semibold text-muted-ink">Tools</p><GrantChipList grants={envelope.spec.tools.map((tool) => ({ kind: grantKindForAction(tool.action), name: `${tool.provider}:${tool.resource}:${tool.action}` }))} /></div>
          </div>
        </SectionCard>
        <SectionCard title="History">
          {request.history.length ? <ol className="space-y-4 border-s ps-5">{request.history.map((event, index) => (
            <li className="relative" key={`${event.at}-${index}`}><span aria-hidden="true" className="absolute -start-[1.58rem] top-1.5 size-2.5 rounded-full border-2 border-brand bg-panel" /><div className="flex flex-wrap items-center gap-2"><StatusBadge value={event.status} /><span className="text-sm text-muted-ink">by {event.actor} · {dateTime(event.at)}</span></div>{event.reason ? <p className="mt-1 text-sm text-muted-ink">{event.reason}</p> : null}</li>
          ))}</ol> : <EmptyState title="No history" />}
        </SectionCard>
      </div>
      {request.status === "provisioned" ? <WorkflowGenerator requestId={request.id} /> : <EmptyState title="Workflow not available"><p>A governed GitHub Actions workflow can be rendered only after this request is provisioned.</p></EmptyState>}
    </div>
  );
}

function WorkflowGenerator({ requestId }: Readonly<{ requestId: string }>) {
  const session = useSession();
  const load = useCallback(() => listPublishedWorkflows(), []);
  const workflows = useApiResource<PublishedWorkflowListResponse>(load);
  const [workflow, setWorkflow] = useState<GithubActionsWorkflowResponse | null>(null);
  const [status, setStatus] = useState<"idle" | "loading" | MutationFailureState>("idle");
  async function submit(event: FormEvent<HTMLFormElement>, published: PublishedWorkflowListResponse["workflows"]) {
    event.preventDefault();
    if (session.status !== "authenticated") return;
    const data = new FormData(event.currentTarget);
    const selected = published.find((item) => workflowReference(item) === String(data.get("workflow")));
    if (!selected) { setStatus("rejected"); return; }
    setStatus("loading");
    const result = await renderWorkflowForEnvelope(session.value.csrf, requestId, selected);
    if (result.data && result.response?.ok) {
      setWorkflow(result.data);
      setStatus("idle");
    } else setStatus(classifyMutationFailure(result.response?.status));
  }
  return (
    <SectionCard title="GitHub Actions workflow">
      <p className="mb-4 text-sm text-muted-ink">Render a published workflow bound to this envelope.</p>
      <ResourceBoundary state={workflows}>{(data) => data.workflows.length === 0 ? <EmptyState title="No data" /> : (
        <form className="grid gap-4 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-end" onSubmit={(event) => void submit(event, data.workflows)}>
          <label className="grid gap-2 text-sm font-semibold">Workflow<select className="min-h-11 rounded-md border bg-panel px-3 font-normal" name="workflow" required>{data.workflows.map((item) => <option key={workflowReference(item)} value={workflowReference(item)}>{item.displayName} · {workflowReference(item)}</option>)}</select></label>
          <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50" disabled={status === "loading"} type="submit">{status === "loading" ? "Rendering…" : "Render workflow"}</button>
        </form>
      )}</ResourceBoundary>
      {status !== "idle" && status !== "loading" ? <p role="alert" className="text-sm text-red-800">{{ conflict: "The envelope changed before the workflow could be rendered. Reload before retrying.", rejected: "Rust rejected the workflow inputs.", forbidden: "The Rust authorization boundary rejected workflow rendering.", unavailable: "The authoritative workflow service is unavailable.", error: "The workflow response could not be accepted." }[status]}</p> : null}
      {workflow ? <div aria-label="Generated workflow" className="mt-4"><CodeBlock code={workflow.workflow.yaml} language="yaml" path={`${workflow.workflow.suggestedPath} · sha256 ${workflow.workflow.sha256.slice(0, 12)}…`} /></div> : null}
    </SectionCard>
  );
}

export function EnvelopeRunsView({ requestId }: Readonly<{ requestId: string }>) {
  const load = useCallback(() => getRequest({ cache: "no-store", credentials: "same-origin", path: { request_id: requestId } }), [requestId]);
  const state = useApiResource<EnvelopeRequestResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="View executions bound to this envelope instance." title="Recent runs" />
      <ResourceBoundary state={state}>{({ request }) => request.envelopeInstanceId ? <EnvelopeRunRecords envelopeInstanceId={request.envelopeInstanceId} /> : <EmptyState title="No envelope instance"><p>This envelope request has not produced an envelope instance.</p></EmptyState>}</ResourceBoundary>
    </section>
  );
}

function EnvelopeRunRecords({ envelopeInstanceId }: Readonly<{ envelopeInstanceId: string }>) {
  const load = useCallback(() => myRuns({ cache: "no-store", credentials: "same-origin", query: { envelopeInstanceId } }), [envelopeInstanceId]);
  const state = useApiResource<MyRunsResponse>(load);
  return <ResourceBoundary state={state}>{({ runs }) => <RunCards runs={runs} />}</ResourceBoundary>;
}
