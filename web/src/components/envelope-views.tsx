"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";

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
import { DefinitionList, EmptyState, PageHeader, PrimaryLink, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { classifyMutationFailure, type MutationFailureState } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";
import { listPublishedWorkflows, renderWorkflowForEnvelope, type PublishedWorkflowListResponse } from "@/workflows/api";
import { workflowReference } from "@/workflows/contracts";

function EnvelopeSummary({ envelope }: Readonly<{ envelope: BrowserEnvelope }>) {
  return <DefinitionList items={[
    ["Monthly limit", `${envelope.spec.budget.monthlyLimit} ${envelope.spec.budget.currency}`],
    ["Single-run limit", envelope.spec.budget.singleRunLimit
      ? `${envelope.spec.budget.singleRunLimit} ${envelope.spec.budget.currency}`
      : "Not set"],
    ["TTL", envelope.spec.ttl],
    ["Models", envelope.spec.llms.length ? envelope.spec.llms.map((model) => `${model.provider}/${model.model}`).join(", ") : "None"],
    ["Tools", envelope.spec.tools.length ? envelope.spec.tools.map((tool) => `${tool.provider}:${tool.resource}:${tool.action}`).join(", ") : "None"],
  ]} />;
}

export function EnvelopesView() {
  const load = useCallback(() => listRequests({ cache: "no-store", credentials: "same-origin" }), []);
  const state = useApiResource<EnvelopeRequestsResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader actions={<PrimaryLink href="/envelopes/new">Request envelope</PrimaryLink>} description="Request and inspect governed runtime authority." title="Envelopes" />
      <ResourceBoundary state={state}>{({ requests }) => requests.length === 0 ? (
        <EmptyState title="No data" />
      ) : (
        <ul className="grid gap-4 lg:grid-cols-2">
          {requests.map((request) => <EnvelopeCard key={request.id} request={request} />)}
        </ul>
      )}</ResourceBoundary>
    </section>
  );
}

function EnvelopeCard({ request }: Readonly<{ request: UserEnvelopeRequest }>) {
  return (
    <li className="rounded-panel border bg-panel p-5 shadow-sm">
      <div className="flex items-start justify-between gap-4">
        <div><h2 className="font-semibold">{request.templateId}</h2><p className="mt-1 break-all font-mono text-xs text-muted-ink">{request.id}</p></div>
        <StatusBadge value={request.status} />
      </div>
      <div className="mt-5"><EnvelopeSummary envelope={request.approvedEnvelope ?? request.requestedEnvelope} /></div>
      <Link className="mt-5 inline-flex min-h-11 items-center text-sm font-semibold text-brand hover:text-brand-strong" href={`/envelopes/${request.id}`}>View envelope →</Link>
    </li>
  );
}

function Accordion({ children, preferenceKey, title }: Readonly<{ children: ReactNode; preferenceKey: string; title: string }>) {
  const details = useRef<HTMLDetailsElement>(null);
  useEffect(() => {
    if (details.current) details.current.open = localStorage.getItem(preferenceKey) === "open";
  }, [preferenceKey]);
  return (
    <details className="rounded-md border" onToggle={(event) => {
      const next = event.currentTarget.open;
      if (next) localStorage.setItem(preferenceKey, "open");
      else localStorage.removeItem(preferenceKey);
    }} ref={details}>
      <summary className="cursor-pointer px-4 py-3 font-semibold">{title}</summary>
      <div className="border-t p-4">{children}</div>
    </details>
  );
}

export function NewEnvelopeView() {
  const load = useCallback(() => listTemplates({ cache: "no-store", credentials: "same-origin" }), []);
  const state = useApiResource<EnvelopeTemplatesResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Choose authority within a server-defined template ceiling." title="New envelope" />
      <ResourceBoundary state={state}>{({ templates }) => templates.length === 0 ? (
        <EmptyState title="No data" />
      ) : <EnvelopeRequestForm templates={templates} />}</ResourceBoundary>
    </section>
  );
}

function EnvelopeRequestForm({ templates }: Readonly<{ templates: Array<AvailableEnvelopeTemplate> }>) {
  const router = useRouter();
  const session = useSession();
  const [templateId, setTemplateId] = useState(templates[0]?.id ?? "");
  const template = templates.find((item) => item.id === templateId) ?? templates[0];
  const [budget, setBudget] = useState(template.ceiling.spec.budget.monthlyLimit);
  const [ttl, setTtl] = useState(template.ceiling.spec.ttl);
  const [models, setModels] = useState(() => new Set(template.ceiling.spec.llms.map((item) => `${item.provider}\u0000${item.model}`)));
  const [tools, setTools] = useState(() => new Set(template.ceiling.spec.tools.map((item) => `${item.provider}\u0000${item.resource}\u0000${item.action}`)));
  const [submission, setSubmission] = useState<"idle" | "submitting" | MutationFailureState>("idle");

  function selectTemplate(id: string) {
    const next = templates.find((item) => item.id === id);
    if (!next) return;
    setTemplateId(id);
    setBudget(next.ceiling.spec.budget.monthlyLimit);
    setTtl(next.ceiling.spec.ttl);
    setModels(new Set(next.ceiling.spec.llms.map((item) => `${item.provider}\u0000${item.model}`)));
    setTools(new Set(next.ceiling.spec.tools.map((item) => `${item.provider}\u0000${item.resource}\u0000${item.action}`)));
    setSubmission("idle");
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated") return;
    setSubmission("submitting");
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
  }

  return (
    <form className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm" onSubmit={submit}>
      <label className="grid gap-2 text-sm font-semibold">Template
        <select className="min-h-11 rounded-md border bg-panel px-3 font-normal" onChange={(event) => selectTemplate(event.target.value)} value={template.id}>
          {templates.map((item) => <option key={item.id} value={item.id}>{item.displayName} · revision {item.revision}</option>)}
        </select>
      </label>
      <div className="grid gap-4 sm:grid-cols-2">
        <label className="grid gap-2 text-sm font-semibold">Monthly limit ({template.ceiling.spec.budget.currency})
          <input className="min-h-11 rounded-md border px-3 font-normal" inputMode="decimal" onChange={(event) => setBudget(event.target.value)} required value={budget} />
        </label>
        <label className="grid gap-2 text-sm font-semibold">Time to live
          <input className="min-h-11 rounded-md border px-3 font-normal" onChange={(event) => setTtl(event.target.value)} required value={ttl} />
        </label>
      </div>
      <Accordion preferenceKey={`steward.ui.envelope-accordion.${template.id}.models`} title="Models">
        <div className="space-y-3">{template.ceiling.spec.llms.map((item) => {
          const key = `${item.provider}\u0000${item.model}`;
          return <label className="flex min-h-11 items-center gap-3 text-sm" key={key}><input checked={models.has(key)} onChange={() => setModels((current) => { const next = new Set(current); if (next.has(key)) next.delete(key); else next.add(key); return next; })} type="checkbox" />{item.provider}/{item.model}</label>;
        })}</div>
      </Accordion>
      <Accordion preferenceKey={`steward.ui.envelope-accordion.${template.id}.tools`} title="Tools">
        <div className="space-y-3">{template.ceiling.spec.tools.map((item) => {
          const key = `${item.provider}\u0000${item.resource}\u0000${item.action}`;
          return <label className="flex min-h-11 items-center gap-3 text-sm" key={key}><input checked={tools.has(key)} onChange={() => setTools((current) => { const next = new Set(current); if (next.has(key)) next.delete(key); else next.add(key); return next; })} type="checkbox" />{item.provider}:{item.resource}:{item.action}</label>;
        })}</div>
      </Accordion>
      {submission !== "idle" && submission !== "submitting" ? <p className="text-sm text-red-800" role="alert">{{ conflict: "The template revision changed. Reload before retrying.", rejected: "Rust admission rejected the requested authority as outside the template ceiling.", forbidden: "The Rust authorization boundary rejected the request.", unavailable: "The authoritative request service is unavailable.", error: "The request could not be accepted." }[submission]}</p> : null}
      <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:cursor-not-allowed disabled:opacity-50" disabled={submission === "submitting"} type="submit">{submission === "submitting" ? "Submitting…" : "Submit request"}</button>
    </form>
  );
}

export function EnvelopeDetailView({ requestId }: Readonly<{ requestId: string }>) {
  const load = useCallback(() => getRequest({ cache: "no-store", credentials: "same-origin", path: { request_id: requestId } }), [requestId]);
  const state = useApiResource<EnvelopeRequestResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Inspect requested and approved authority without filling data gaps." title="Envelope" />
      <ResourceBoundary state={state}>{({ request }) => <EnvelopeDetail request={request} />}</ResourceBoundary>
    </section>
  );
}

function EnvelopeDetail({ request }: Readonly<{ request: UserEnvelopeRequest }>) {
  return (
    <div className="space-y-5">
      <article className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm">
        <div className="flex flex-wrap justify-between gap-3"><div><h2 className="text-xl font-semibold">{request.templateId}</h2><p className="mt-1 break-all font-mono text-xs text-muted-ink">{request.id}</p></div><StatusBadge value={request.status} /></div>
        <EnvelopeSummary envelope={request.approvedEnvelope ?? request.requestedEnvelope} />
        {request.reason ? <p className="rounded-md bg-notice p-4 text-sm"><strong>Server reason:</strong> {request.reason}</p> : null}
        {request.envelopeInstanceId ? <PrimaryLink href={`/envelopes/${request.id}/runs`}>View recent runs</PrimaryLink> : null}
      </article>
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
    if (result.data && result.response?.ok) { setWorkflow(result.data); setStatus("idle"); } else setStatus(classifyMutationFailure(result.response?.status));
  }
  return (
    <section className="space-y-4 rounded-panel border bg-panel p-6 shadow-sm" aria-labelledby="workflow-title">
      <div><h2 className="text-xl font-semibold" id="workflow-title">GitHub Actions workflow</h2><p className="mt-1 text-sm text-muted-ink">Generate a workflow using a published Steward Workflow and this Envelope.</p></div>
      <ResourceBoundary state={workflows}>{(data) => data.workflows.length === 0 ? <EmptyState title="No data" /> : (
        <form className="grid gap-4 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-end" onSubmit={(event) => void submit(event, data.workflows)}>
          <label className="grid gap-2 text-sm font-semibold">Workflow<select className="min-h-11 rounded-md border bg-panel px-3 font-normal" name="workflow" required>{data.workflows.map((item) => <option key={workflowReference(item)} value={workflowReference(item)}>{item.displayName} · {workflowReference(item)}</option>)}</select></label>
          <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50" disabled={status === "loading"} type="submit">{status === "loading" ? "Rendering…" : "Render workflow"}</button>
        </form>
      )}</ResourceBoundary>
      {status !== "idle" && status !== "loading" ? <p role="alert" className="text-sm text-red-800">{{ conflict: "The envelope changed before the workflow could be rendered. Reload before retrying.", rejected: "Rust rejected the workflow inputs.", forbidden: "The Rust authorization boundary rejected workflow rendering.", unavailable: "The authoritative workflow service is unavailable.", error: "The workflow response could not be accepted." }[status]}</p> : null}
      {workflow ? <div className="space-y-2"><p className="text-xs text-muted-ink">SHA-256: <span className="break-all font-mono">{workflow.workflow.sha256}</span></p><textarea aria-label="Generated workflow" className="min-h-80 w-full rounded-md border bg-canvas p-4 font-mono text-xs" readOnly value={workflow.workflow.yaml} /></div> : null}
    </section>
  );
}

export function EnvelopeRunsView({ requestId }: Readonly<{ requestId: string }>) {
  const load = useCallback(() => getRequest({ cache: "no-store", credentials: "same-origin", path: { request_id: requestId } }), [requestId]);
  const state = useApiResource<EnvelopeRequestResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="View executions bound to this envelope instance." title="Recent runs" />
      <ResourceBoundary state={state}>{({ request }) => request.envelopeInstanceId ? <EnvelopeRunRecords runtimeUid={request.envelopeInstanceId} /> : <EmptyState title="No runtime instance"><p>This envelope request has not produced a runtime instance.</p></EmptyState>}</ResourceBoundary>
    </section>
  );
}

function EnvelopeRunRecords({ runtimeUid }: Readonly<{ runtimeUid: string }>) {
  const load = useCallback(() => myRuns({ cache: "no-store", credentials: "same-origin", query: { runtimeUid } }), [runtimeUid]);
  const state = useApiResource<MyRunsResponse>(load);
  return <ResourceBoundary state={state}>{({ runs }) => <RunCards runs={runs} />}</ResourceBoundary>;
}
