"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useState, type FormEvent } from "react";

import {
  authorAdminEnvelopeTemplate,
  getAdminCapabilities,
  getAdminEnvelopeTemplate,
  type BrowserEnvelope,
  type BrowserEnvelopeTemplateResponse,
  type CapabilityCatalog,
  type ModelRef,
  type RunnerPlatform,
  type ToolGrant,
} from "@/api-client";
import { EmptyState, PageHeader, ResourceBoundary } from "@/components/workspace-ui";
import { classifyMutationFailure } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

type TemplateMutationState = "idle" | "saving" | "saved" | "conflict" | "rejected" | "forbidden" | "unavailable" | "error";
type LimitType = "singleRun" | "monthly";

type AdminTemplateListItem = {
  memberRole: string;
  envelope: BrowserEnvelope;
};

type AdminTemplateListResponse = {
  apiVersion: "steward.browser-admin/v1";
  templates: Array<AdminTemplateListItem>;
};

const fieldClass = "min-h-11 min-w-0 w-full rounded-md border bg-panel px-3 font-normal";

export const initialEnvelopeTemplate: BrowserEnvelope = {
  revision: 1,
  spec: {
    budget: {
      currency: "USD",
      monthlyLimit: "0.10",
      singleRunLimit: "0.10",
    },
    llms: [],
    tools: [],
    ttl: "15m",
    runner: { platforms: ["linux"] },
  },
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function isBrowserEnvelope(value: unknown): value is BrowserEnvelope {
  if (!isRecord(value)) return false;
  const envelope = value;
  if (typeof envelope.revision !== "number" || !Number.isSafeInteger(envelope.revision) || !isRecord(envelope.spec)) return false;
  const spec = envelope.spec;
  const validBudget = isRecord(spec.budget)
    && typeof spec.budget.currency === "string"
    && typeof spec.budget.monthlyLimit === "string"
    && (spec.budget.singleRunLimit === undefined
      || spec.budget.singleRunLimit === null
      || typeof spec.budget.singleRunLimit === "string");
  const validModels = Array.isArray(spec.llms)
    && spec.llms.every((model) => isRecord(model) && typeof model.provider === "string" && typeof model.model === "string");
  const validTools = Array.isArray(spec.tools)
    && spec.tools.every((tool) => isRecord(tool)
      && typeof tool.provider === "string"
      && typeof tool.resource === "string"
      && typeof tool.action === "string");
  const validRunner = spec.runner === undefined || (isRecord(spec.runner)
    && (spec.runner.platforms === undefined || (Array.isArray(spec.runner.platforms)
      && spec.runner.platforms.every((platform) => platform === "linux" || platform === "mac" || platform === "windows")))
    && (spec.runner.memory === undefined || typeof spec.runner.memory === "string")
    && (spec.runner.compute === undefined || typeof spec.runner.compute === "string")
    && (spec.runner.storage === undefined || typeof spec.runner.storage === "string"));
  return validBudget && validModels && validTools && validRunner && typeof spec.ttl === "string";
}

function normalizeEnvelopeTemplateResponse(value: unknown, selectedRole: string): BrowserEnvelopeTemplateResponse | null {
  if (!isRecord(value)) return null;
  if (value.apiVersion === "steward.browser-admin/v1"
    && value.memberRole === selectedRole
    && isBrowserEnvelope(value.envelope)) {
    return value as BrowserEnvelopeTemplateResponse;
  }
  if (value.apiVersion !== "steward.admin/v1" || !isRecord(value.template)) return null;
  const template = value.template;
  if (template.id !== selectedRole
    || typeof template.revision !== "number"
    || !Number.isSafeInteger(template.revision)
    || !isBrowserEnvelope(template.envelope)
    || template.revision !== template.envelope.revision) {
    return null;
  }
  return {
    apiVersion: "steward.browser-admin/v1",
    memberRole: selectedRole,
    envelope: template.envelope,
  };
}

function normalizeTemplateList(value: unknown): AdminTemplateListResponse | null {
  if (!isRecord(value) || value.apiVersion !== "steward.browser-admin/v1" || !Array.isArray(value.templates)) return null;
  const templates: Array<AdminTemplateListItem> = [];
  for (const item of value.templates) {
    if (!isRecord(item) || typeof item.memberRole !== "string" || !isBrowserEnvelope(item.envelope)) return null;
    templates.push({ memberRole: item.memberRole, envelope: item.envelope });
  }
  return { apiVersion: "steward.browser-admin/v1", templates };
}

function normalizeCapabilityCatalog(value: unknown): CapabilityCatalog | null {
  if (!isRecord(value)
    || value.schemaVersion !== "steward.capability-catalog/v1"
    || !Array.isArray(value.models)
    || !Array.isArray(value.tools)) return null;
  const modelsValid = value.models.every((model) => isRecord(model)
    && typeof model.provider === "string"
    && typeof model.model === "string");
  const toolsValid = value.tools.every((tool) => isRecord(tool)
    && typeof tool.provider === "string"
    && typeof tool.resource === "string"
    && typeof tool.action === "string");
  return modelsValid && toolsValid ? value as CapabilityCatalog : null;
}

async function getAdminEnvelopeTemplates(): Promise<{ data?: unknown; response?: Response }> {
  try {
    const response = await fetch("/admin/api/v1/envelope-templates", {
      cache: "no-store",
      credentials: "same-origin",
    });
    return response.ok ? { data: await response.json(), response } : { response };
  } catch {
    return {};
  }
}

function displayName(memberRole: string): string {
  return memberRole
    .split(/[-_\s]+/)
    .filter(Boolean)
    .map((part) => `${part.charAt(0).toUpperCase()}${part.slice(1)}`)
    .join(" ");
}

function modelValue(model: ModelRef): string {
  return `${model.provider}/${model.model}`;
}

function modelKey(model: ModelRef): string {
  return JSON.stringify([model.provider, model.model]);
}

function modelLabel(model: ModelRef, choices: Array<ModelRef>): string {
  const display = modelValue(model);
  return choices.some((other) => modelKey(other) !== modelKey(model) && modelValue(other) === display)
    ? `Provider: ${model.provider} · Model: ${model.model}`
    : display;
}

function initialTemplateForCatalog(capabilities: CapabilityCatalog): BrowserEnvelope {
  return {
    ...initialEnvelopeTemplate,
    spec: {
      ...initialEnvelopeTemplate.spec,
      llms: capabilities.models.slice(0, 1),
    },
  };
}

function toolValue(tool: ToolGrant): string {
  return `${tool.provider}:${tool.resource}:${tool.action}`;
}

function toolKey(tool: ToolGrant): string {
  return JSON.stringify([tool.provider, tool.resource, tool.action]);
}

function toolLabel(tool: ToolGrant, choices: Array<ToolGrant>): string {
  const display = `${tool.resource}:${tool.action}`;
  return choices.some((other) => toolKey(other) !== toolKey(tool) && `${other.resource}:${other.action}` === display)
    ? `Resource: ${tool.resource} · Action: ${tool.action}`
    : display;
}

function mutationMessage(status: Exclude<TemplateMutationState, "idle" | "saving">): string {
  return {
    saved: "Template revision accepted by the Rust authority.",
    conflict: "A newer template revision already exists. Load it before authoring another revision.",
    rejected: "The template ID or envelope fields are invalid, so no authority was changed.",
    forbidden: "The Rust authorization boundary rejected this template mutation.",
    unavailable: "The authoritative template service is unavailable.",
    error: "The template response could not be accepted.",
  }[status];
}

export function AdminEnvelopeTemplatesView() {
  const session = useSession();
  if (session.status !== "authenticated") {
    return <EmptyState title="Session unavailable"><p>The authoritative administrator session is not available.</p></EmptyState>;
  }
  return <AuthenticatedTemplateList />;
}

function AuthenticatedTemplateList() {
  const load = useCallback(() => getAdminEnvelopeTemplates(), []);
  const state = useApiResource<unknown>(load);
  const acceptedState = state.status === "ready"
    ? normalizeTemplateList(state.value)
      ? { status: "ready" as const, value: normalizeTemplateList(state.value)! }
      : { status: "error" as const }
    : state;

  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader
        actions={<Link className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white shadow-sm hover:bg-brand-strong" href="/admin/envelopes/templates/new">Create template</Link>}
        description="Review the current immutable envelope templates available in Steward."
        title="Envelope templates"
      />
      <ResourceBoundary state={acceptedState}>{({ templates }) => templates.length === 0 ? (
        <EmptyState title="No data" />
      ) : (
        <ul className="grid gap-3" role="list">
          {templates.map(({ memberRole, envelope }) => (
            <li key={memberRole}>
              <Link className="flex min-h-20 items-center justify-between gap-4 rounded-panel border bg-panel px-5 py-4 shadow-sm hover:border-brand" href={`/admin/envelopes/templates/${encodeURIComponent(memberRole)}`}>
                <span>
                  <span className="block font-semibold">{displayName(memberRole)}</span>
                  <span className="mt-1 block text-sm text-muted-ink">{memberRole}</span>
                </span>
                <span className="text-sm text-muted-ink">Revision {envelope.revision}</span>
              </Link>
            </li>
          ))}
        </ul>
      )}</ResourceBoundary>
    </section>
  );
}

export function AdminEnvelopeTemplateDetailView({ memberRole }: Readonly<{ memberRole: string }>) {
  const session = useSession();
  if (session.status !== "authenticated") {
    return <EmptyState title="Session unavailable"><p>The authoritative administrator session is not available.</p></EmptyState>;
  }
  return <AuthenticatedTemplateDetail csrf={session.value.csrf} memberRole={memberRole} />;
}

function AuthenticatedTemplateDetail({ csrf, memberRole }: Readonly<{ csrf: string; memberRole: string }>) {
  const load = useCallback(() => getAdminEnvelopeTemplate({
    cache: "no-store",
    credentials: "same-origin",
    path: { member_role: memberRole },
  }), [memberRole]);
  const state = useApiResource<BrowserEnvelopeTemplateResponse>(load);
  const loadCapabilities = useCallback(() => getAdminCapabilities({
    cache: "no-store",
    credentials: "same-origin",
  }), []);
  const capabilitiesState = useApiResource<CapabilityCatalog>(loadCapabilities);
  const normalizedTemplate = state.status === "ready"
    ? normalizeEnvelopeTemplateResponse(state.value, memberRole)
    : null;
  const acceptedState = state.status === "ready"
    ? normalizedTemplate
      ? { status: "ready" as const, value: normalizedTemplate }
      : { status: "error" as const }
    : state;
  const acceptedCapabilitiesState = capabilitiesState.status === "ready"
    ? normalizeCapabilityCatalog(capabilitiesState.value)
      ? { status: "ready" as const, value: normalizeCapabilityCatalog(capabilitiesState.value)! }
      : { status: "error" as const }
    : capabilitiesState;

  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader
        actions={<Link className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas" href="/admin/envelopes/templates">All templates</Link>}
        description="Inspect the current immutable revision and author a successor."
        title="Envelope template"
      />
      <ResourceBoundary state={acceptedState}>{({ envelope }) => (
        <ResourceBoundary state={acceptedCapabilitiesState}>{(capabilities) => (
          <TemplateEditor
            capabilities={capabilities}
            csrf={csrf}
            key={`${memberRole}:${envelope.revision}:${capabilities.models.length}:${capabilities.tools.length}`}
            memberRole={memberRole}
            template={envelope}
          />
        )}</ResourceBoundary>
      )}</ResourceBoundary>
    </section>
  );
}

export function AdminNewEnvelopeTemplateView() {
  const session = useSession();
  if (session.status !== "authenticated") {
    return <EmptyState title="Session unavailable"><p>The authoritative administrator session is not available.</p></EmptyState>;
  }
  return <AuthenticatedNewTemplate csrf={session.value.csrf} />;
}

function AuthenticatedNewTemplate({ csrf }: Readonly<{ csrf: string }>) {
  const load = useCallback(() => getAdminCapabilities({
    cache: "no-store",
    credentials: "same-origin",
  }), []);
  const state = useApiResource<CapabilityCatalog>(load);
  const acceptedState = state.status === "ready"
    ? normalizeCapabilityCatalog(state.value)
      ? { status: "ready" as const, value: normalizeCapabilityCatalog(state.value)! }
      : { status: "error" as const }
    : state;
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader
        actions={<Link className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas" href="/admin/envelopes/templates">All templates</Link>}
        description="Author the first immutable revision for a member role. All suggested values remain editable before saving."
        title="Create envelope template"
      />
      <ResourceBoundary state={acceptedState}>{(capabilities) => (
        <TemplateEditor
          capabilities={capabilities}
          create
          csrf={csrf}
          key={`new:${capabilities.models.length}:${capabilities.tools.length}`}
          memberRole=""
          template={initialTemplateForCatalog(capabilities)}
        />
      )}</ResourceBoundary>
    </section>
  );
}

function TemplateEditor({ capabilities, create = false, csrf, memberRole, template }: Readonly<{ capabilities: CapabilityCatalog; create?: boolean; csrf: string; memberRole: string; template: BrowserEnvelope }>) {
  const router = useRouter();
  const modelCatalog = capabilities.models;
  const allowedModels = new Set(modelCatalog.map(modelKey));
  const toolCatalog = capabilities.tools;
  const allowedTools = new Set(toolCatalog.map(toolKey));
  const toolProviders = [...new Set(toolCatalog.map((tool) => tool.provider))];
  const [status, setStatus] = useState<TemplateMutationState>("idle");
  const [currentRevision, setCurrentRevision] = useState(template.revision);
  const [models, setModels] = useState<Array<ModelRef>>(template.spec.llms);
  const [modelInput, setModelInput] = useState(modelCatalog[0] ? modelKey(modelCatalog[0]) : "");
  const [tools, setTools] = useState<Array<ToolGrant>>(template.spec.tools);
  const [toolProviderInput, setToolProviderInput] = useState(toolProviders[0] ?? "");
  const [toolInput, setToolInput] = useState(toolCatalog[0] ? toolKey(toolCatalog[0]) : "");
  const [limitType, setLimitType] = useState<LimitType>("singleRun");
  const [monthlyLimit, setMonthlyLimit] = useState(template.spec.budget.monthlyLimit);
  const [singleRunLimit, setSingleRunLimit] = useState(template.spec.budget.singleRunLimit ?? "");
  const limitAmount = limitType === "singleRun" ? singleRunLimit : monthlyLimit;

  function addModel() {
    const selected = modelCatalog.find((model) => modelKey(model) === modelInput);
    if (!selected || models.some((model) => modelKey(model) === modelInput)) return;
    setModels([...models, selected]);
  }

  function removeModel(index: number) {
    setModels(models.filter((_, modelIndex) => modelIndex !== index));
  }

  function addTool() {
    const selected = toolCatalog.find((tool) => toolKey(tool) === toolInput && tool.provider === toolProviderInput);
    if (!selected || tools.some((tool) => toolKey(tool) === toolInput)) return;
    setTools([...tools, selected]);
  }

  function removeTool(index: number) {
    setTools(tools.filter((_, toolIndex) => toolIndex !== index));
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const fields = new FormData(event.currentTarget);
    const submitter = (event.nativeEvent as SubmitEvent).submitter;
    const action = submitter instanceof HTMLButtonElement ? submitter.value : (create ? "create" : "version");
    const saveAsNew = action === "copy";
    const newTemplateId = String(fields.get("newTemplateId") ?? "").trim();
    const targetRole = create || saveAsNew ? newTemplateId : memberRole;
    const platforms = fields.getAll("platforms").filter((value): value is RunnerPlatform =>
      value === "linux" || value === "mac" || value === "windows");
    if (!targetRole
      || !monthlyLimit.trim()
      || !singleRunLimit.trim()
      || models.length === 0
      || models.some((model) => !allowedModels.has(modelKey(model)))
      || tools.some((tool) => !allowedTools.has(toolKey(tool)))) {
      setStatus("rejected");
      return;
    }
    const memory = String(fields.get("memory") ?? "").trim();
    const compute = String(fields.get("compute") ?? "").trim();
    const storage = String(fields.get("storage") ?? "").trim();
    const body: BrowserEnvelope = {
      revision: create || saveAsNew ? 1 : currentRevision + 1,
      spec: {
        budget: {
          currency: "USD",
          monthlyLimit: monthlyLimit.trim(),
          singleRunLimit: singleRunLimit.trim(),
        },
        llms: models,
        tools,
        ttl: String(fields.get("ttl") ?? "").trim(),
        runner: {
          platforms,
          ...(memory ? { memory } : {}),
          ...(compute ? { compute } : {}),
          ...(storage ? { storage } : {}),
        },
      },
    };
    setStatus("saving");
    const result = await authorAdminEnvelopeTemplate({
      body,
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": csrf },
      path: { member_role: targetRole },
    });
    if (result.data && result.response?.status === 201) {
      setCurrentRevision(result.data.envelope.revision);
      setStatus("saved");
      if (create || saveAsNew) router.push(`/admin/envelopes/templates/${encodeURIComponent(targetRole)}`);
      return;
    }
    setStatus(classifyMutationFailure(result.response?.status));
  }

  const runner = template.spec.runner;
  return (
    <form className="space-y-6 rounded-panel border bg-panel p-6 shadow-sm" onSubmit={submit}>
      <div>
        <h2 className="text-xl font-semibold">{create ? "New template" : displayName(memberRole)}</h2>
        <p className="mt-1 text-sm text-muted-ink">{create ? "Initial revision 1" : `Current revision ${currentRevision}`}</p>
      </div>

      <fieldset className="grid gap-4 sm:grid-cols-4">
        <legend className="mb-3 text-base font-semibold">Inference usage and TTL</legend>
        <label className="grid gap-2 text-sm font-semibold">Currency
          <select className={fieldClass} defaultValue="USD" name="currency"><option value="USD">USD</option></select>
        </label>
        <label className="grid gap-2 text-sm font-semibold">Limit type
          <select className={fieldClass} onChange={(event) => setLimitType(event.target.value as LimitType)} value={limitType}>
            <option value="singleRun">Single run</option>
            <option value="monthly">Monthly</option>
          </select>
        </label>
        <label className="grid gap-2 text-sm font-semibold">Limit amount (USD)
          <input
            className={fieldClass}
            inputMode="decimal"
            onChange={(event) => {
              if (limitType === "singleRun") setSingleRunLimit(event.target.value);
              else setMonthlyLimit(event.target.value);
            }}
            required
            value={limitAmount}
          />
        </label>
        <label className="grid gap-2 text-sm font-semibold">TTL
          <input className={fieldClass} defaultValue={template.spec.ttl} name="ttl" required />
        </label>
        <p className="text-sm text-muted-ink sm:col-span-4">
          Single run: {singleRunLimit || "Not set"} USD · Monthly: {monthlyLimit || "Not set"} USD
        </p>
      </fieldset>

      <fieldset className="space-y-3">
        <legend className="text-base font-semibold">Models</legend>
        <div className="flex flex-wrap items-end gap-3">
          <label className="grid min-w-64 flex-1 gap-2 text-sm font-semibold">Model
            <select className={fieldClass} disabled={modelCatalog.length === 0} onChange={(event) => setModelInput(event.target.value)} value={modelInput}>
              {modelCatalog.length === 0
                ? <option value="">No models available</option>
                : modelCatalog.map((model) => {
                  const key = modelKey(model);
                  return <option key={key} value={key}>{modelLabel(model, modelCatalog)}</option>;
                })}
            </select>
          </label>
          <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-50" disabled={!modelInput || models.some((model) => modelKey(model) === modelInput)} onClick={addModel} type="button">Add model</button>
        </div>
        {modelCatalog.length === 0 ? <p className="text-sm text-muted-ink">No models are listed in the deployment capability catalog.</p> : null}
        <ul className="flex flex-wrap gap-2" role="list">
          {models.map((model, index) => {
            const allowed = allowedModels.has(modelKey(model));
            return (
              <li className={allowed ? "flex items-center gap-2 rounded-full border px-3 py-1.5 text-sm" : "flex items-center gap-2 rounded-full border px-3 py-1.5 text-sm text-muted-ink"} key={`${modelKey(model)}:${index}`}>
                <span>{modelLabel(model, models)}</span>
                {!allowed ? <span className="text-xs">Not listed in the deployment capability catalog</span> : null}
                <button aria-label={`Remove model ${model.model} from provider ${model.provider}`} className="rounded-full p-1 hover:bg-canvas" onClick={() => removeModel(index)} type="button">
                  <svg aria-hidden="true" className="size-3" fill="none" stroke="currentColor" strokeWidth="2" viewBox="0 0 12 12">
                    <path d="M2 2l8 8M10 2l-8 8" />
                  </svg>
                </button>
              </li>
            );
          })}
        </ul>
      </fieldset>

      <fieldset className="space-y-3">
        <legend className="text-base font-semibold">Tools</legend>
        <div className="grid gap-3 sm:grid-cols-[1fr_2fr_auto] sm:items-end">
          <label className="grid gap-2 text-sm font-semibold">Tool provider
            <select className={fieldClass} disabled={toolProviders.length === 0} onChange={(event) => {
              const provider = event.target.value;
              setToolProviderInput(provider);
              const first = toolCatalog.find((tool) => tool.provider === provider);
              setToolInput(first ? toolKey(first) : "");
            }} value={toolProviderInput}>
              {toolProviders.length === 0
                ? <option value="">No tool providers available</option>
                : toolProviders.map((provider) => <option key={provider} value={provider}>{provider === "github" ? "GitHub" : provider}</option>)}
            </select>
          </label>
          <label className="grid gap-2 text-sm font-semibold">Tool
            <select className={fieldClass} disabled={toolCatalog.length === 0} onChange={(event) => setToolInput(event.target.value)} value={toolInput}>
              {toolCatalog.length === 0
                ? <option value="">No tools available</option>
                : toolCatalog.filter((tool) => tool.provider === toolProviderInput).map((tool) => <option key={toolKey(tool)} value={toolKey(tool)}>{toolLabel(tool, toolCatalog)}</option>)}
            </select>
          </label>
          <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-50" disabled={!toolInput || tools.some((tool) => toolKey(tool) === toolInput)} onClick={addTool} type="button">Add tool</button>
        </div>
        {toolCatalog.length === 0 ? <p className="text-sm text-muted-ink">No tools are listed in the deployment capability catalog.</p> : null}
        <ul className="flex flex-wrap gap-2" role="list">
          {tools.map((tool, index) => {
            const allowed = allowedTools.has(toolKey(tool));
            return (
              <li className={allowed ? "flex items-center gap-2 rounded-full border px-3 py-1.5 text-sm" : "flex items-center gap-2 rounded-full border px-3 py-1.5 text-sm text-muted-ink"} key={`${toolKey(tool)}:${index}`}>
                <span>{toolValue(tool)}</span>
                {!allowed ? <span className="text-xs">Not listed in the deployment capability catalog</span> : null}
                <button aria-label={`Remove tool ${toolValue(tool)}`} className="rounded-full p-1 hover:bg-canvas" onClick={() => removeTool(index)} type="button">
                  <svg aria-hidden="true" className="size-3" fill="none" stroke="currentColor" strokeWidth="2" viewBox="0 0 12 12">
                    <path d="M2 2l8 8M10 2l-8 8" />
                  </svg>
                </button>
              </li>
            );
          })}
        </ul>
      </fieldset>

      <details className="rounded-md border p-4">
        <summary className="cursor-pointer font-semibold">Advanced</summary>
        <fieldset className="mt-5 space-y-4">
          <legend className="text-base font-semibold">Runner config</legend>
          <div className="flex flex-wrap gap-5">
            {(["linux", "mac", "windows"] as const).map((platform) => (
              <label className="flex min-h-11 items-center gap-2 text-sm capitalize" key={platform}>
                <input defaultChecked={runner?.platforms?.includes(platform)} name="platforms" type="checkbox" value={platform} />{platform}
              </label>
            ))}
          </div>
          <div className="grid gap-4 sm:grid-cols-3">
            <label className="grid gap-2 text-sm font-semibold">Memory<input className={fieldClass} defaultValue={runner?.memory ?? ""} name="memory" placeholder="2Gi" /></label>
            <label className="grid gap-2 text-sm font-semibold">Compute<input className={fieldClass} defaultValue={runner?.compute ?? ""} name="compute" placeholder="1000m" /></label>
            <label className="grid gap-2 text-sm font-semibold">Storage<input className={fieldClass} defaultValue={runner?.storage ?? ""} name="storage" placeholder="10Gi" /></label>
          </div>
        </fieldset>
      </details>

      {status !== "idle" && status !== "saving" ? (
        <p className={status === "saved" ? "text-sm text-green-800" : "text-sm text-red-800"} role={status === "saved" ? "status" : "alert"}>{mutationMessage(status)}</p>
      ) : null}

      <div className="flex flex-wrap items-end gap-3 border-t pt-5">
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50" disabled={status === "saving"} name="action" type="submit" value={create ? "create" : "version"}>{status === "saving" ? "Saving…" : create ? "Create template" : "Save new version"}</button>
        <label className="grid min-w-56 flex-1 gap-2 text-sm font-semibold">{create ? "Template ID" : "New template ID"}
          <input className={fieldClass} name="newTemplateId" placeholder="developer" required={create} />
        </label>
        {!create ? <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas disabled:opacity-50" disabled={status === "saving"} name="action" type="submit" value="copy">Save as new</button> : null}
      </div>
    </form>
  );
}
