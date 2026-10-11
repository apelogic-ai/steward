"use client";

import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { useCallback, useRef, useState, type FormEvent } from "react";

import {
  createRequest,
  getRequest,
  listTemplates,
  myRuns,
  type AvailableEnvelopeTemplate,
  type BrowserEnvelope,
  type EnvelopeRequestErrorResponse,
  type EnvelopeRequestField,
  type EnvelopeRequestResponse,
  type EnvelopeTemplatesResponse,
  type GithubActionsWorkflowResponse,
  type MyRunsResponse,
  type UserEnvelopeRequest,
} from "@/api-client";
import { RunCards } from "@/components/run-views";
import { DiagnosticDetails, type DiagnosticItem } from "@/components/diagnostic-details";
import { EnvelopeExceedanceSummary, type EnvelopeExceedanceSummaryValue, envelopeDeltaSummary } from "@/components/envelope-exceedance-summary";
import { CodeBlock, DataTable, FilterTabs, GrantChipList, Meter, SectionCard, StatStrip, TagSelect, grantKindForAction } from "@/components/hs";
import { DurationInput, MoneyInput, durationUnit, formatCount, formatDuration, formatMoney } from "@/components/presentation-values";
import { WorkspaceAuthorityDetails } from "@/components/workspace-authority-details";
import { EmptyState, PageHeader, PrimaryLink, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { classifyMutationFailure, type MutationFailureState } from "@/data/mutation-state";
import { loadAllEnvelopeRequests } from "@/data/paginated-api";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";
import { listPublishedWorkflows, renderWorkflowForEnvelope, type PublishedWorkflowListResponse } from "@/workflows/api";
import { workflowReference } from "@/workflows/contracts";

function dateTime(value: string): string {
  const parsed = new Date(value);
  return Number.isNaN(parsed.valueOf()) ? value : parsed.toLocaleString();
}

function decimalParts(value: string): { fractional: string; integer: string } | null {
  if (!/^[0-9]+(?:\.[0-9]*)?$/.test(value)) return null;
  const [integer = "", fractional = ""] = value.split(".");
  return { integer: integer.replace(/^0+/, ""), fractional: fractional.replace(/0+$/, "") };
}

function decimalExceeds(value: string, ceiling: string): boolean {
  const left = decimalParts(value);
  const right = decimalParts(ceiling);
  if (!left || !right) return false;
  if (left.integer.length !== right.integer.length) return left.integer.length > right.integer.length;
  if (left.integer !== right.integer) return left.integer > right.integer;
  const width = Math.max(left.fractional.length, right.fractional.length);
  return left.fractional.padEnd(width, "0") > right.fractional.padEnd(width, "0");
}

function envelopeRequestProblem(value: unknown): EnvelopeRequestErrorResponse | null {
  if (typeof value !== "object" || value === null || !("failureReason" in value)) return null;
  const problem = value as EnvelopeRequestErrorResponse;
  return typeof problem.failureReason === "string" && problem.failureReason.trim() && Array.isArray(problem.fields) ? problem : null;
}

function durationSeconds(value: string): bigint | null {
  const match = /^([0-9]+)(s|m|h|d)$/.exec(value);
  if (!match) return null;
  const multiplier = { s: 1n, m: 60n, h: 3600n, d: 86400n }[match[2] as "s" | "m" | "h" | "d"];
  return BigInt(match[1]) * multiplier;
}

function durationExceeds(value: string, ceiling: string): boolean {
  const requested = durationSeconds(value);
  const maximum = durationSeconds(ceiling);
  return requested !== null && maximum !== null && requested > maximum;
}

export function templateRequestReviewSummary({ budget, models, runtimeMinutes, singleRunLimit, template, tools, ttl }: Readonly<{
  budget: string;
  models: ReadonlySet<string>;
  runtimeMinutes: string;
  singleRunLimit: string;
  template: AvailableEnvelopeTemplate;
  tools: ReadonlySet<string>;
  ttl: string;
}>): { exceedances: Array<EnvelopeExceedanceSummaryValue>; requiresApproval: boolean } {
  const exceedances = [
    decimalExceeds(budget, template.ceiling.spec.budget.monthlyLimit) ? { ceiling: formatMoney(template.ceiling.spec.budget.monthlyLimit, template.ceiling.spec.budget.currency), label: "Monthly limit", requested: formatMoney(budget, template.ceiling.spec.budget.currency) } : null,
    template.ceiling.spec.budget.singleRunLimit && singleRunLimit.trim() && decimalExceeds(singleRunLimit.trim(), template.ceiling.spec.budget.singleRunLimit) ? { ceiling: formatMoney(template.ceiling.spec.budget.singleRunLimit, template.ceiling.spec.budget.currency), label: "Limit per run", requested: formatMoney(singleRunLimit.trim(), template.ceiling.spec.budget.currency) } : null,
    template.ceiling.spec.runtimeMinutesLimit && runtimeMinutes.trim() && decimalExceeds(runtimeMinutes.trim(), template.ceiling.spec.runtimeMinutesLimit) ? { ceiling: formatDuration(template.ceiling.spec.runtimeMinutesLimit, "minutes", "m"), label: "Runtime limit per month", requested: formatDuration(runtimeMinutes.trim(), "minutes", "m") } : null,
    durationExceeds(ttl, template.ceiling.spec.ttl) ? { ceiling: formatDuration(template.ceiling.spec.ttl, "wire", durationUnit(ttl)), label: "Time-to-live limit", requested: formatDuration(ttl, "wire", durationUnit(ttl)) } : null,
    [...models].some((model) => !template.ceiling.spec.llms.some((allowed) => `${allowed.provider}\u0000${allowed.model}` === model)) ? {
      ceiling: template.ceiling.spec.llms.map((model) => `${model.provider}/${model.model}`).join(", ") || "None",
      label: "Models",
      requested: [...models].map((model) => model.replace("\u0000", "/")).join(", ") || "None",
    } : null,
    [...tools].some((tool) => !template.ceiling.spec.tools.some((allowed) => `${allowed.provider}\u0000${allowed.resource}\u0000${allowed.action}` === tool)) ? {
      ceiling: template.ceiling.spec.tools.map((tool) => `${tool.provider}:${tool.resource}:${tool.action}`).join(", ") || "None",
      label: "Tools",
      requested: [...tools].map((tool) => tool.replaceAll("\u0000", ":")).join(", ") || "None",
    } : null,
  ].filter((value): value is EnvelopeExceedanceSummaryValue => value !== null);
  return { exceedances, requiresApproval: exceedances.length > 0 };
}

export function EnvelopesView() {
  const [status, setStatus] = useState("all");
  const load = useCallback(async () => {
    const [requests, templates] = await Promise.all([
      loadAllEnvelopeRequests(),
      listTemplates({ cache: "no-store", credentials: "same-origin" }),
    ]);
    return {
      data: requests.data && templates.data ? { requests: requests.data.requests, templates: templates.data.templates } : undefined,
      response: !requests.response?.ok ? requests.response : templates.response,
    };
  }, []);
  const state = useApiResource<{ requests: Array<UserEnvelopeRequest>; templates: Array<AvailableEnvelopeTemplate> }>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader actions={<PrimaryLink href="/envelopes/new">Request envelope</PrimaryLink>} description="Budget, models and tools your agents are allowed to use." title="Envelopes" />
      <ResourceBoundary state={state}>{({ requests, templates }) => {
        if (requests.length === 0) return <p className="rounded-card border bg-panel p-6 text-sm text-muted-ink">No envelopes yet.</p>;
        const count = (value: string) => requests.filter((request) => value === "pending" ? request.status !== "provisioned" && request.status !== "rejected" : request.status === value).length;
        const filtered = status === "all" ? requests : requests.filter((request) => status === "pending" ? request.status !== "provisioned" && request.status !== "rejected" : request.status === status);
        const names = new Map(templates.map((template) => [template.id, template.displayName]));
        return <div className="space-y-5">
          <FilterTabs active={status} items={[
            { count: requests.length, label: "All", value: "all" },
            { count: count("provisioned"), label: "Provisioned", value: "provisioned" },
            { count: count("pending"), label: "Pending", value: "pending" },
            { count: count("rejected"), label: "Rejected", value: "rejected" },
          ]} onChange={setStatus} />
          <EnvelopeTable names={names} requests={filtered} />
        </div>;
      }}</ResourceBoundary>
    </section>
  );
}

function EnvelopeTable({ names, requests }: Readonly<{ names: ReadonlyMap<string, string>; requests: Array<UserEnvelopeRequest> }>) {
  if (requests.length === 0) return <EmptyState title="No matching envelopes" />;
  return <DataTable
    ariaLabel="Envelopes"
    columns={[
      { key: "name", label: "Template", className: "font-semibold", render: (request) => <span><span className="block truncate">{request.templateId ? names.get(request.templateId) ?? request.templateId : "Custom envelope"}</span><span className="mt-0.5 block truncate font-mono text-xs font-normal text-muted-ink">{request.id}</span></span> },
      { key: "status", label: "Status", render: (request) => <StatusBadge value={request.status} /> },
      { key: "usage", label: "Spend this month", render: (request) => {
        const spend = request.usage?.spend;
        return spend ? <Meter limit={Number(spend.limit)} limitLabel={formatMoney(spend.limit, spend.currency)} used={Number(spend.observed)} usedLabel={formatMoney(spend.observed, spend.currency)} /> : <span className="text-faint-ink">—</span>;
      } },
      { key: "per-run", label: "Limit per run", className: "tabular-nums", render: (request) => {
        const spec = (request.approvedEnvelope ?? request.requestedEnvelope).spec;
        return spec.budget.singleRunLimit ? formatMoney(spec.budget.singleRunLimit, spec.budget.currency) : "—";
      } },
      { key: "ttl", label: "Time-to-live limit", className: "font-mono text-muted-ink", render: (request) => formatDuration((request.approvedEnvelope ?? request.requestedEnvelope).spec.ttl) },
      { key: "tools", label: "Tools", className: "tabular-nums text-muted-ink", render: (request) => formatCount((request.approvedEnvelope ?? request.requestedEnvelope).spec.tools.length, "tool") },
      { key: "open", label: "", className: "text-right text-lg text-faint-ink", render: () => "›" },
    ]}
    gridTemplateColumns="minmax(220px,1.5fr) 120px minmax(190px,1fr) 110px 80px 70px 24px"
    minWidth="900px"
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
      <PageHeader description="Pick an existing template and adjust it, or create your own envelope request." title="Request an envelope" />
      <ResourceBoundary state={state}>{({ templates }) => <EnvelopeRequestForm templates={templates} />}</ResourceBoundary>
    </section>
  );
}

function EnvelopeRequestForm({ templates }: Readonly<{ templates: Array<AvailableEnvelopeTemplate> }>) {
  const searchParams = useSearchParams();
  const requestedTemplate = searchParams.get("template");
  const initialTemplate = templates.find((template) => template.id === requestedTemplate) ?? templates[0];
  const [selection, setSelection] = useState(() => searchParams.get("type") === "custom" || !initialTemplate ? "custom" : `template:${initialTemplate.id}`);
  const [submitting, setSubmitting] = useState(false);
  const template = selection.startsWith("template:") ? templates.find((candidate) => `template:${candidate.id}` === selection) : undefined;
  return (
    <div className="space-y-5">
      <label className="grid min-w-0 gap-2 text-sm font-semibold">Template
        <select className="min-h-11 min-w-0 w-full max-w-full rounded-md border bg-panel px-3 font-normal" disabled={submitting} onChange={(event) => setSelection(event.target.value)} value={template ? `template:${template.id}` : "custom"}>
          {templates.map((candidate) => <option key={candidate.id} value={`template:${candidate.id}`}>{candidate.displayName} · rev {candidate.revision} — monthly limit {formatMoney(candidate.ceiling.spec.budget.monthlyLimit, candidate.ceiling.spec.budget.currency)} · time-to-live limit {formatDuration(candidate.ceiling.spec.ttl)} · {formatCount(candidate.ceiling.spec.tools.length, "tool")}</option>)}
          <option value="custom">Custom envelope</option>
        </select>
      </label>
      {template
        ? <TemplateEnvelopeRequestForm key={template.id} onSubmittingChange={setSubmitting} template={template} />
        : <CustomEnvelopeRequestForm onSubmittingChange={setSubmitting} />}
    </div>
  );
}

function TemplateEnvelopeRequestForm({ onSubmittingChange, template }: Readonly<{ onSubmittingChange: (submitting: boolean) => void; template: AvailableEnvelopeTemplate }>) {
  const router = useRouter();
  const session = useSession();
  const [budget, setBudget] = useState(template.ceiling.spec.budget.monthlyLimit);
  const [singleRunLimit, setSingleRunLimit] = useState(template.ceiling.spec.budget.singleRunLimit ?? "");
  const [runtimeMinutes, setRuntimeMinutes] = useState(template.ceiling.spec.runtimeMinutesLimit ?? "");
  const [ttl, setTtl] = useState(template.ceiling.spec.ttl);
  const [models, setModels] = useState(() => new Set(template.ceiling.spec.llms.map((item) => `${item.provider}\u0000${item.model}`)));
  const [tools, setTools] = useState(() => new Set(template.ceiling.spec.tools.map((item) => `${item.provider}\u0000${item.resource}\u0000${item.action}`)));
  const [justification, setJustification] = useState("");
  const [submission, setSubmission] = useState<"idle" | "submitting" | MutationFailureState>("idle");
  const [rejectedFields, setRejectedFields] = useState<Array<EnvelopeRequestField>>([]);
  const [diagnostics, setDiagnostics] = useState<DiagnosticItem[]>([]);

  function clearRejectedSubmission() {
    if (submission === "idle" || submission === "submitting") return;
    setSubmission("idle");
    setRejectedFields([]);
    setDiagnostics([]);
  }

  const singleRunValue = singleRunLimit.trim();
  const singleRunError = !singleRunValue && template.ceiling.spec.budget.singleRunLimit
    ? "Enter a limit per run."
    : singleRunValue && !decimalParts(singleRunValue)
      ? "Limit per run must be 0 or more."
      : singleRunValue && decimalExceeds(singleRunValue, budget)
        ? "Limit per run cannot exceed the monthly limit."
        : null;

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated" || singleRunError || models.size === 0) return;
    setSubmission("submitting");
    setRejectedFields([]);
    onSubmittingChange(true);
    const result = await createRequest({
      body: {
        idempotencyKey: crypto.randomUUID(),
        templateId: template.id,
        templateRevision: template.revision,
        justification: justification.trim() || null,
        requestedEnvelope: {
          revision: template.revision,
          spec: {
            ...template.ceiling.spec,
            budget: {
              currency: template.ceiling.spec.budget.currency,
              monthlyLimit: budget,
              ...(singleRunValue ? { singleRunLimit: singleRunValue } : {}),
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
    const problem = envelopeRequestProblem(result.error);
    setRejectedFields(problem?.fields ?? []);
    setDiagnostics(problem ? [{ label: "Code", value: problem.error }, { label: "Reason", value: problem.failureReason }] : []);
    setSubmission(classifyMutationFailure(result.response?.status));
    onSubmittingChange(false);
  }

  const modelOptions = template.ceiling.spec.llms.map((item) => ({ key: `${item.provider}\u0000${item.model}`, kind: "model" as const, label: `${item.provider}/${item.model}` }));
  const toolOptions = template.ceiling.spec.tools.map((item) => ({ key: `${item.provider}\u0000${item.resource}\u0000${item.action}`, kind: grantKindForAction(item.action), label: `${item.provider}:${item.resource}:${item.action}` }));
  const { exceedances, requiresApproval } = templateRequestReviewSummary({ budget, models, runtimeMinutes, singleRunLimit, template, tools, ttl });
  const exceededFields = new Set(exceedances.map(({ label }) => label));
  if (rejectedFields.includes("budgetMonthlyLimit")) exceededFields.add("Monthly limit");
  if (rejectedFields.includes("budgetSingleRunLimit")) exceededFields.add("Limit per run");
  if (rejectedFields.includes("runtimeMinutesLimit")) exceededFields.add("Runtime limit per month");
  if (rejectedFields.includes("ttl")) exceededFields.add("Time-to-live limit");
  const fieldClass = (exceeded: boolean) => exceeded ? "border-warn ring-1 ring-warn" : "";
  const limitHint = (exceeded: boolean, value: string) => <span className={`text-xs font-normal ${exceeded ? "text-warn" : "text-muted-ink"}`}>{exceeded ? "Above limit" : "Limit"} {value}</span>;
  const stepTitle = (number: string, title: string) => <span className="flex items-center gap-3"><span className="font-mono text-xs font-normal text-faint-ink">{number}</span>{title}</span>;
  const summary = [
    ["Template", template.displayName],
    ["Monthly limit", formatMoney(budget, template.ceiling.spec.budget.currency)],
    ["Limit per run", singleRunValue ? formatMoney(singleRunValue, template.ceiling.spec.budget.currency) : "Not set"],
    ["Time-to-live limit", formatDuration(ttl)],
    ["Models", formatCount(models.size, "model")],
    ["Tools", formatCount(tools.size, "tool")],
  ];
  return (
    <form className="flex flex-wrap items-start gap-5" onSubmit={submit}>
      <div className="min-w-0 flex-[1_1_520px] space-y-4">
        <SectionCard title={stepTitle("01", "Budget and lifetime")}>
          <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
            <label className="grid gap-2 text-sm font-semibold">Monthly limit<MoneyInput aria-invalid={exceededFields.has("Monthly limit") || undefined} className={fieldClass(exceededFields.has("Monthly limit"))} currency={template.ceiling.spec.budget.currency} disabled={submission === "submitting"} onChange={(value) => { setBudget(value); clearRejectedSubmission(); }} required value={budget} />{limitHint(exceededFields.has("Monthly limit"), formatMoney(template.ceiling.spec.budget.monthlyLimit, template.ceiling.spec.budget.currency))}</label>
            <label className="grid gap-2 text-sm font-semibold">Limit per run<MoneyInput aria-invalid={Boolean(singleRunError) || exceededFields.has("Limit per run")} className={fieldClass(exceededFields.has("Limit per run") || Boolean(singleRunError))} currency={template.ceiling.spec.budget.currency} disabled={submission === "submitting"} onChange={(value) => { setSingleRunLimit(value); clearRejectedSubmission(); }} required={Boolean(template.ceiling.spec.budget.singleRunLimit)} value={singleRunLimit} />{template.ceiling.spec.budget.singleRunLimit ? limitHint(exceededFields.has("Limit per run"), formatMoney(template.ceiling.spec.budget.singleRunLimit, template.ceiling.spec.budget.currency)) : <span className="text-xs font-normal text-muted-ink">Optional · cannot exceed the monthly limit</span>}{singleRunError ? <span className="text-xs font-normal text-err" role="alert">{singleRunError}</span> : null}</label>
            <label className="grid gap-2 text-sm font-semibold">Time-to-live limit<DurationInput aria-invalid={exceededFields.has("Time-to-live limit") || undefined} className={fieldClass(exceededFields.has("Time-to-live limit"))} disabled={submission === "submitting"} onChange={(value) => { setTtl(value); clearRejectedSubmission(); }} required unitAriaLabel="Time-to-live unit" value={ttl} />{limitHint(exceededFields.has("Time-to-live limit"), formatDuration(template.ceiling.spec.ttl, "wire", durationUnit(ttl)))}</label>
            {template.ceiling.spec.runtimeMinutesLimit ? <label className="grid gap-2 text-sm font-semibold">Runtime limit per month<DurationInput aria-invalid={exceededFields.has("Runtime limit per month") || undefined} className={fieldClass(exceededFields.has("Runtime limit per month"))} disabled={submission === "submitting"} mode="minutes" onChange={(value) => { setRuntimeMinutes(value); clearRejectedSubmission(); }} required value={runtimeMinutes} />{limitHint(exceededFields.has("Runtime limit per month"), formatDuration(template.ceiling.spec.runtimeMinutesLimit, "minutes", "m"))}</label> : null}
          </div>
        </SectionCard>
        <SectionCard className={models.size === 0 || exceededFields.has("Models") || rejectedFields.includes("llms") ? "border-warn ring-1 ring-warn" : undefined} title={stepTitle("02", "Models")}><TagSelect aria-invalid={models.size === 0 || undefined} addPlaceholder="Add…" disabled={submission === "submitting"} emptyPlaceholder="Search models…" label="Models" noResultsMessage={(query) => `No models in this template match “${query}”`} onChange={(next) => { setModels(new Set(next)); clearRejectedSubmission(); }} options={modelOptions} value={[...models]} />{models.size === 0 ? <p className="mt-2 text-sm text-err" role="alert">Select at least one model.</p> : null}</SectionCard>
        <SectionCard className={exceededFields.has("Tools") || rejectedFields.includes("tools") ? "border-warn ring-1 ring-warn" : undefined} title={stepTitle("03", "Tools")}><TagSelect addPlaceholder="Add another tool…" disabled={submission === "submitting"} emptyPlaceholder="Search GitHub tools…" label="Tools" noResultsMessage={(query) => `No tools in this template match “${query}”`} onChange={(next) => { setTools(new Set(next)); clearRejectedSubmission(); }} options={toolOptions} value={[...tools]} /><p className="mt-3 text-xs text-muted-ink">Remove anything this envelope doesn’t need. Only tools allowed by the template are offered.</p></SectionCard>
        <SectionCard title={stepTitle("04", "Request note")}><label className="grid gap-2 text-sm font-semibold">Justification (optional)<textarea className="min-h-24 rounded-control border p-3 font-normal" disabled={submission === "submitting"} maxLength={2000} onChange={(event) => { setJustification(event.target.value); clearRejectedSubmission(); }} placeholder="Explain why these limits or capabilities are needed." value={justification} /></label></SectionCard>
      </div>
      <SectionCard className="sticky top-7 flex-[1_1_280px] lg:max-w-[380px]" title="Summary">
        <dl className="space-y-3 text-sm">{summary.map(([label, value]) => <div className={`flex justify-between gap-4 ${exceededFields.has(label) ? "text-warn" : ""}`} key={label}><dt className={exceededFields.has(label) ? undefined : "text-muted-ink"}>{label}</dt><dd className="text-right font-medium">{value}</dd></div>)}</dl>
        {submission !== "idle" && submission !== "submitting" ? <p className="mt-4 text-sm text-err" role="alert">{{ conflict: "The template revision changed. Reload before retrying.", rejected: "Some values are outside the platform limits or available capabilities. Check the highlighted fields and try again.", forbidden: "You are not allowed to submit this request.", unavailable: "HyperShell couldn't reach the request service. Try again in a moment.", error: "The request could not be accepted. Try again." }[submission]}</p> : null}
        <DiagnosticDetails className="mt-2" items={diagnostics} />
        {exceedances.length ? <div className="mt-4"><EnvelopeExceedanceSummary values={exceedances} /></div> : null}
        <button className="mt-5 min-h-10 w-full rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:cursor-not-allowed disabled:opacity-50" disabled={submission === "submitting" || Boolean(singleRunError) || models.size === 0} type="submit">{submission === "submitting" ? "Submitting…" : requiresApproval ? "Submit for approval" : "Submit request"}</button>
        <p className="mt-3 text-xs leading-5 text-muted-ink">{requiresApproval ? `${exceedances.length} ${exceedances.length === 1 ? "value exceeds" : "values exceed"} the template limits and need admin approval.` : "This request is within the template limits and will be provisioned immediately."}</p>
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
  const [justification, setJustification] = useState("");
  const [submission, setSubmission] = useState<"idle" | "submitting" | "invalid" | MutationFailureState>("idle");
  const [diagnostics, setDiagnostics] = useState<DiagnosticItem[]>([]);
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
        justification: justification.trim() || null,
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
    const problem = envelopeRequestProblem(result.error);
    setDiagnostics(problem ? [{ label: "Code", value: problem.error }, { label: "Reason", value: problem.failureReason }] : []);
    onSubmittingChange(false);
  }

  const failure = submission === "invalid"
    ? "Enter a complete envelope JSON object with its revision, limits, models, tools, and time-to-live limit."
    : submission !== "idle" && submission !== "submitting"
      ? {
          conflict: "An equivalent envelope request already exists. Reload before retrying.",
          rejected: "The custom request must fit the platform-wide limits and use available models and tools.",
          forbidden: "You are not allowed to submit this request.",
          unavailable: "HyperShell couldn't submit the request. Try again in a moment.",
          error: "The request could not be accepted. Try again.",
        }[submission]
      : null;

  return (
    <form className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm" onSubmit={submit}>
      <div>
        <h2 className="font-semibold">Custom envelope request</h2>
        <p className="mt-1 text-sm text-muted-ink">Custom requests have no governing template and always require administrator approval. HyperShell validates the complete envelope against the deployment safety ceiling; model, tool, and limit values are specific to this deployment.</p>
      </div>
      <label className="grid gap-2 text-sm font-semibold">Complete envelope JSON
        <textarea
          className="min-h-80 rounded-md border bg-canvas p-4 font-mono text-xs font-normal"
          disabled={submission === "submitting"}
          onChange={(event) => { setEnvelopeJson(event.target.value); idempotencyKey.current = null; setSubmission("idle"); setDiagnostics([]); }}
          placeholder="Paste envelope JSON."
          required
          spellCheck={false}
          value={envelopeJson}
        />
      </label>
      <label className="grid gap-2 text-sm font-semibold">Justification (optional)
        <textarea className="min-h-24 rounded-control border p-3 font-normal" disabled={submission === "submitting"} maxLength={2000} onChange={(event) => { setJustification(event.target.value); idempotencyKey.current = null; setSubmission("idle"); }} placeholder="Explain why this envelope is needed." value={justification} />
      </label>
      {failure ? <p className="text-sm text-red-800" role="alert">{failure}</p> : null}
      <DiagnosticDetails items={diagnostics} />
      <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:cursor-not-allowed disabled:opacity-50" disabled={submission === "submitting"} type="submit">{submission === "submitting" ? "Submitting…" : "Submit request"}</button>
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
          value: usage ? formatMoney(usage.observed, usage.currency) : "Not reported",
          detail: usage ? <Meter limit={Number(usage.limit)} limitLabel={formatMoney(usage.limit, usage.currency)} used={Number(usage.observed)} usedLabel={formatMoney(usage.observed, usage.currency)} /> : request.usage?.availability.reason ? <details><summary className="cursor-pointer">Details</summary><p className="mt-1">{request.usage.availability.reason}</p></details> : undefined,
        },
        { label: "Limit per run", value: envelope.spec.budget.singleRunLimit ? formatMoney(envelope.spec.budget.singleRunLimit, envelope.spec.budget.currency) : "Not set" },
        { label: "Time-to-live limit", value: <span className="font-mono">{formatDuration(envelope.spec.ttl)}</span> },
        { label: "Envelope instance", value: <span className="font-mono text-sm">{request.envelopeInstanceId ?? "Not assigned"}</span> },
      ]} />
      {request.reason ? <p className="rounded-card bg-notice p-4 text-sm"><strong>Reason:</strong> {request.reason}</p> : null}
      {request.status === "pending" ? <SectionCard title="Pending administrator approval">
        <EnvelopeExceedanceSummary values={request.ceilingExceedances.map(envelopeDeltaSummary)} />
        {!request.ceilingExceedances.length ? <p className="text-sm">{request.templateId ? "This request is awaiting administrator approval." : "This custom envelope request needs administrator approval."}</p> : null}
        {request.justification ? <p className="mt-4 text-sm"><strong>Justification:</strong> {request.justification}</p> : null}
      </SectionCard> : null}
      <div className="grid gap-5 lg:grid-cols-2">
        <SectionCard title={request.approvedEnvelope ? "Approved access" : "Requested access"}>
          <div className="space-y-5">
            <div><p className="mb-2 text-xs font-semibold text-muted-ink">Models</p><GrantChipList grants={envelope.spec.llms.map((model) => ({ kind: "model", name: `${model.provider}/${model.model}` }))} /></div>
            <div><p className="mb-2 text-xs font-semibold text-muted-ink">Tools</p><GrantChipList grants={envelope.spec.tools.map((tool) => ({ kind: grantKindForAction(tool.action), name: `${tool.provider}:${tool.resource}:${tool.action}` }))} /></div>
            <div><p className="mb-2 text-xs font-semibold text-muted-ink">Workspace</p><WorkspaceAuthorityDetails workspace={envelope.spec.workspace} /></div>
          </div>
        </SectionCard>
        <SectionCard title="History">
          {request.history.length ? <ol className="space-y-4 border-s ps-5">{request.history.map((event, index) => (
            <li className="relative" key={`${event.at}-${index}`}><span aria-hidden="true" className="absolute -start-[1.58rem] top-1.5 size-2.5 rounded-full border-2 border-brand bg-panel" /><div className="flex flex-wrap items-center gap-2"><StatusBadge value={event.status} /><span className="text-sm text-muted-ink">by {event.actor} · {dateTime(event.at)}</span></div>{event.reason ? <p className="mt-1 text-sm text-muted-ink">{event.reason}</p> : null}</li>
          ))}</ol> : <EmptyState title="No history" />}
        </SectionCard>
      </div>
      {request.status === "provisioned" ? <WorkflowGenerator requestId={request.id} /> : <EmptyState title="Workflow not available"><p>A GitHub Actions workflow can be prepared only after this request is provisioned.</p></EmptyState>}
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
      <ResourceBoundary state={workflows}>{(data) => data.workflows.length === 0 ? <p className="text-sm text-muted-ink">No published workflows yet.</p> : (
        <form className="grid gap-4 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-end" onSubmit={(event) => void submit(event, data.workflows)}>
          <label className="grid gap-2 text-sm font-semibold">Workflow<select className="min-h-11 rounded-md border bg-panel px-3 font-normal" name="workflow" required>{data.workflows.map((item) => <option key={workflowReference(item)} value={workflowReference(item)}>{item.displayName} · {workflowReference(item)}</option>)}</select></label>
          <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={status === "loading"} type="submit">{status === "loading" ? "Rendering…" : "Render workflow"}</button>
        </form>
      )}</ResourceBoundary>
      {status !== "idle" && status !== "loading" ? <p role="alert" className="text-sm text-red-800">{{ conflict: "The envelope changed before the workflow could be rendered. Reload before retrying.", rejected: "The workflow inputs were rejected.", forbidden: "You are not allowed to render this workflow.", unavailable: "Workflow rendering is unavailable. Try again.", error: "The workflow response could not be accepted." }[status]}</p> : null}
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
