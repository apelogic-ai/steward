"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useState, type FormEvent } from "react";

import {
  getAdminCapabilities,
  getAdminEnvelopeTemplate,
  putAdminEnvelopeTemplate,
  type BrowserEnvelope,
  type BrowserEnvelopeTemplateResponse,
  type CapabilityCatalog,
  type ModelRef,
  type RunnerPlatform,
  type ToolGrant,
} from "@/api-client";
import { DataTable, FormSection, GrantChipList, SectionCard, TagSelect, grantKindForAction } from "@/components/hs";
import { EmptyState, PageHeader, ResourceBoundary } from "@/components/workspace-ui";
import { classifyMutationFailure } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

type TemplateMutationState = "idle" | "saving" | "saved" | "conflict" | "rejected" | "forbidden" | "unavailable" | "error";

type AdminTemplateListItem = {
  autoProvisionThreshold?: BrowserEnvelope | null;
  id: string;
  displayName: string;
  memberRoles: Array<string>;
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
    runtimeMinutesLimit: "60",
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
  const validRuntimeMinutes = spec.runtimeMinutesLimit === undefined
    || spec.runtimeMinutesLimit === null
    || typeof spec.runtimeMinutesLimit === "string";
  return validBudget && validModels && validTools && validRunner && validRuntimeMinutes && typeof spec.ttl === "string";
}

function isAutoProvisionThreshold(value: unknown): value is BrowserEnvelope | null | undefined {
  return value === undefined || value === null || isBrowserEnvelope(value);
}

function normalizeEnvelopeTemplateResponse(value: unknown, templateId: string): BrowserEnvelopeTemplateResponse | null {
  if (!isRecord(value)) return null;
  if (value.apiVersion === "steward.browser-admin/v1"
    && value.id === templateId
    && typeof value.displayName === "string"
    && Array.isArray(value.memberRoles)
    && value.memberRoles.every((role) => typeof role === "string")
    && isAutoProvisionThreshold(value.autoProvisionThreshold)
    && isBrowserEnvelope(value.envelope)) {
    return value as BrowserEnvelopeTemplateResponse;
  }
  // Accept the pre-catalog response during a rolling deployment.
  if (value.apiVersion === "steward.browser-admin/v1"
    && value.memberRole === templateId
    && isBrowserEnvelope(value.envelope)) {
    return {
      apiVersion: "steward.browser-admin/v1",
      id: templateId,
      displayName: displayName(templateId),
      memberRole: templateId,
      memberRoles: [templateId],
      envelope: value.envelope,
      autoProvisionThreshold: null,
    };
  }
  if (value.apiVersion !== "steward.admin/v1" || !isRecord(value.template)) return null;
  const template = value.template;
  if (template.id !== templateId
    || typeof template.revision !== "number"
    || !Number.isSafeInteger(template.revision)
    || !isBrowserEnvelope(template.envelope)
    || template.revision !== template.envelope.revision) {
    return null;
  }
  return {
    apiVersion: "steward.browser-admin/v1",
    id: templateId,
    displayName: typeof template.displayName === "string" ? template.displayName : displayName(templateId),
    memberRole: templateId,
    memberRoles: Array.isArray(template.memberRoles) && template.memberRoles.every((role) => typeof role === "string")
      ? template.memberRoles
      : [templateId],
    envelope: template.envelope,
    autoProvisionThreshold: isAutoProvisionThreshold(template.autoProvisionThreshold) ? template.autoProvisionThreshold : null,
  };
}

function normalizeTemplateList(value: unknown): AdminTemplateListResponse | null {
  if (!isRecord(value) || value.apiVersion !== "steward.browser-admin/v1" || !Array.isArray(value.templates)) return null;
  const templates: Array<AdminTemplateListItem> = [];
  for (const item of value.templates) {
    if (!isRecord(item) || !isBrowserEnvelope(item.envelope)) return null;
    if (typeof item.id === "string" && typeof item.displayName === "string"
      && Array.isArray(item.memberRoles) && item.memberRoles.every((role) => typeof role === "string")) {
      if (!isAutoProvisionThreshold(item.autoProvisionThreshold)) return null;
      templates.push({ id: item.id, displayName: item.displayName, memberRoles: item.memberRoles, envelope: item.envelope, autoProvisionThreshold: item.autoProvisionThreshold });
    } else if (typeof item.memberRole === "string") {
      templates.push({ id: item.memberRole, displayName: displayName(item.memberRole), memberRoles: [item.memberRole], envelope: item.envelope, autoProvisionThreshold: null });
    } else return null;
  }
  return { apiVersion: "steward.browser-admin/v1", templates };
}

function normalizeCapabilityCatalog(value: unknown): CapabilityCatalog | null {
  if (!isRecord(value)
    || value.schemaVersion !== "steward.capability-catalog/v2"
    || !Array.isArray(value.models)
    || !Array.isArray(value.tools)
    || !Array.isArray(value.catalogs)) return null;
  const modelsValid = value.models.every((model) => isRecord(model)
    && typeof model.provider === "string"
    && typeof model.model === "string");
  const toolsValid = value.tools.every((tool) => isRecord(tool)
      && typeof tool.provider === "string"
      && typeof tool.resource === "string"
      && typeof tool.action === "string"
      && (tool.accessClass === "read" || tool.accessClass === "write" || tool.accessClass === "destructive"));
  const catalogsValid = value.catalogs.every((catalog) => isRecord(catalog)
    && typeof catalog.provider === "string"
    && typeof catalog.catalogId === "string"
    && typeof catalog.version === "string"
    && typeof catalog.available === "boolean");
  return modelsValid && toolsValid && catalogsValid ? value as CapabilityCatalog : null;
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
        actions={<div className="flex flex-wrap gap-3">
          <Link className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas" href="/admin/envelopes/provision">Provision envelope</Link>
          <Link className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-on-brand shadow-sm hover:bg-brand-strong" href="/admin/envelopes/templates/new">Create template</Link>
        </div>}
        description="The most a member role can request. Each save creates a new immutable revision."
        title="Envelope templates"
      />
      <ResourceBoundary state={acceptedState}>{({ templates }) => templates.length === 0 ? (
        <p className="rounded-card border bg-panel p-6 text-sm text-muted-ink">No templates yet.</p>
      ) : (
        <DataTable
          ariaLabel="Envelope templates"
          columns={[
            { key: "template", label: "Member role", className: "font-semibold", render: (template) => <span><span className="block truncate">{template.displayName}</span><span className="mt-0.5 block truncate font-mono text-xs font-normal text-muted-ink">{template.id}</span></span> },
            { key: "revision", label: "Revision", className: "font-mono text-muted-ink", render: (template) => `rev ${template.envelope.revision}` },
            { key: "monthly", label: "Monthly", className: "tabular-nums", render: (template) => `${template.envelope.spec.budget.monthlyLimit} ${template.envelope.spec.budget.currency}` },
            { key: "per-run", label: "Per run", className: "tabular-nums", render: (template) => template.envelope.spec.budget.singleRunLimit ? `${template.envelope.spec.budget.singleRunLimit} ${template.envelope.spec.budget.currency}` : "—" },
            { key: "ttl", label: "TTL", className: "font-mono text-muted-ink", render: (template) => template.envelope.spec.ttl },
            { key: "tools", label: "Tools", render: (template) => <GrantChipList grants={template.envelope.spec.tools.map((tool) => ({ kind: grantKindForAction(tool.action), name: toolValue(tool) }))} limit={2} /> },
            { key: "open", label: "", className: "text-right text-lg text-faint-ink", render: () => "›" },
          ]}
          gridTemplateColumns="minmax(200px,1.2fr) 90px 120px 120px 80px minmax(210px,1fr) 24px"
          minWidth="980px"
          rowHref={(template) => `/admin/envelopes/templates/${encodeURIComponent(template.id)}`}
          rowKey={(template) => template.id}
          rows={templates}
        />
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
    path: { template_id: memberRole },
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
      <ResourceBoundary state={acceptedState}>{({ autoProvisionThreshold, displayName: templateDisplayName, envelope, memberRoles }) => (
        <ResourceBoundary state={acceptedCapabilitiesState}>{(capabilities) => (
          <TemplateEditor
            capabilities={capabilities}
            csrf={csrf}
            key={`${memberRole}:${envelope.revision}:${capabilities.models.length}:${capabilities.tools.length}`}
            memberRole={memberRole}
            memberRoles={memberRoles}
            templateDisplayName={templateDisplayName}
            template={envelope}
            autoProvisionThreshold={autoProvisionThreshold}
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
        description="Sets the ceiling for one member role. This becomes revision 1."
        title="Create envelope template"
      />
      <ResourceBoundary state={acceptedState}>{(capabilities) => (
        <TemplateEditor
          capabilities={capabilities}
          create
          csrf={csrf}
          key={`new:${capabilities.models.length}:${capabilities.tools.length}`}
          memberRole=""
          memberRoles={[]}
          templateDisplayName=""
          template={initialTemplateForCatalog(capabilities)}
          autoProvisionThreshold={null}
        />
      )}</ResourceBoundary>
    </section>
  );
}

function TemplateEditor({ autoProvisionThreshold, capabilities, create = false, csrf, memberRole, memberRoles, templateDisplayName, template }: Readonly<{ autoProvisionThreshold?: BrowserEnvelope | null; capabilities: CapabilityCatalog; create?: boolean; csrf: string; memberRole: string; memberRoles: Array<string>; templateDisplayName: string; template: BrowserEnvelope }>) {
  const router = useRouter();
  const modelCatalog = capabilities.models;
  const allowedModels = new Set(modelCatalog.map(modelKey));
  const toolCatalog = capabilities.tools;
  const allowedTools = new Set(toolCatalog.map(toolKey));
  const [status, setStatus] = useState<TemplateMutationState>("idle");
  const [currentRevision, setCurrentRevision] = useState(template.revision);
  const [models, setModels] = useState<Array<ModelRef>>(template.spec.llms);
  const [tools, setTools] = useState<Array<ToolGrant>>(template.spec.tools.map((tool) => ({
    provider: tool.provider,
    resource: tool.resource,
    action: tool.action,
  })));
  const [monthlyLimit, setMonthlyLimit] = useState(template.spec.budget.monthlyLimit);
  const [singleRunLimit, setSingleRunLimit] = useState(template.spec.budget.singleRunLimit ?? "");
  const [runtimeMinutesLimit, setRuntimeMinutesLimit] = useState(template.spec.runtimeMinutesLimit ?? "");
  const [name, setName] = useState(templateDisplayName);
  const [roles, setRoles] = useState(memberRoles.join(", "));
  const [autoApproveToCeiling, setAutoApproveToCeiling] = useState(autoProvisionThreshold === null || autoProvisionThreshold === undefined);
  const [thresholdJson, setThresholdJson] = useState(JSON.stringify(autoProvisionThreshold ?? template, null, 2));
  const missingModels = models.filter((model) => !allowedModels.has(modelKey(model)));
  const missingTools = tools.filter((tool) => !allowedTools.has(toolKey(tool)));
  const modelOptions = [...modelCatalog, ...missingModels].map((model) => ({
    disabled: !allowedModels.has(modelKey(model)),
    key: modelKey(model),
    kind: "model" as const,
    label: modelLabel(model, [...modelCatalog, ...missingModels]),
    note: allowedModels.has(modelKey(model)) ? undefined : "Not listed in the deployment capability catalog",
  }));
  const toolOptions = [...toolCatalog, ...missingTools].map((tool) => {
    const catalogTool = toolCatalog.find((candidate) => toolKey(candidate) === toolKey(tool));
    return {
      disabled: !catalogTool,
      key: toolKey(tool),
      kind: catalogTool?.accessClass ?? grantKindForAction(tool.action),
      label: toolLabel(tool, [...toolCatalog, ...missingTools]),
      note: catalogTool ? displayName(tool.provider) : "Not listed in the deployment capability catalog",
    };
  });

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const fields = new FormData(event.currentTarget);
    const submitter = (event.nativeEvent as SubmitEvent).submitter;
    const action = submitter instanceof HTMLButtonElement ? submitter.value : (create ? "create" : "version");
    const saveAsNew = action === "copy";
    const newTemplateId = String(fields.get("newTemplateId") ?? "").trim();
    const templateId = create || saveAsNew ? newTemplateId : memberRole;
    const selectedRoles = [...new Set(roles.split(",").map((role) => role.trim()).filter(Boolean))].sort();
    const platforms = fields.getAll("platforms").filter((value): value is RunnerPlatform =>
      value === "linux" || value === "mac" || value === "windows");
    if (!templateId
      || !name.trim()
      || selectedRoles.length === 0
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
    const envelope: BrowserEnvelope = {
      revision: create || saveAsNew ? 1 : currentRevision + 1,
      spec: {
        budget: {
          currency: "USD",
          monthlyLimit: monthlyLimit.trim(),
          singleRunLimit: singleRunLimit.trim(),
        },
        llms: models,
        tools,
        ...(runtimeMinutesLimit.trim() ? { runtimeMinutesLimit: runtimeMinutesLimit.trim() } : {}),
        ttl: String(fields.get("ttl") ?? "").trim(),
        runner: {
          platforms,
          ...(memory ? { memory } : {}),
          ...(compute ? { compute } : {}),
          ...(storage ? { storage } : {}),
        },
      },
    };
    let nextAutoProvisionThreshold: BrowserEnvelope | null = null;
    if (!autoApproveToCeiling) {
      try {
        const parsed: unknown = JSON.parse(thresholdJson);
        if (!isBrowserEnvelope(parsed)) { setStatus("rejected"); return; }
        nextAutoProvisionThreshold = parsed;
      } catch {
        setStatus("rejected");
        return;
      }
    }
    setStatus("saving");
    const result = await putAdminEnvelopeTemplate({
      body: {
        displayName: name.trim(),
        memberRoles: selectedRoles,
        envelope,
        autoProvisionThreshold: nextAutoProvisionThreshold,
      },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": csrf },
      path: { template_id: templateId },
    });
    if (result.data && result.response?.status === 201) {
      setCurrentRevision(result.data.envelope.revision);
      setStatus("saved");
      if (create || saveAsNew) router.push(`/admin/envelopes/templates/${encodeURIComponent(templateId)}`);
      return;
    }
    setStatus(classifyMutationFailure(result.response?.status));
  }

  const runner = template.spec.runner;
  if (create) {
    return (
      <form className="overflow-hidden rounded-card border bg-panel" onSubmit={submit}>
        <FormSection description="The template ID. Users with this role can request against it." title="Member role">
          <div className="grid gap-4 sm:grid-cols-3">
            <label className="grid gap-2 text-sm font-semibold">Template ID<input className={`${fieldClass} font-mono`} name="newTemplateId" placeholder="developer" required /></label>
            <label className="grid gap-2 text-sm font-semibold">Display name<input className={fieldClass} name="displayName" onChange={(event) => setName(event.target.value)} required value={name} /></label>
            <label className="grid gap-2 text-sm font-semibold">Eligible member roles<TagSelect addPlaceholder="Add another role…" allowCreate emptyPlaceholder="Add member role…" label="Eligible member roles" onChange={(next) => setRoles(next.join(", "))} options={roles.split(",").map((role) => role.trim()).filter(Boolean).map((role) => ({ key: role, kind: "neutral", label: role }))} value={roles.split(",").map((role) => role.trim()).filter(Boolean)} /></label>
          </div>
        </FormSection>
        <FormSection description="Inference spend limits in USD and how long an envelope stays valid." title="Budget and lifetime">
          <div className="grid gap-4 sm:grid-cols-4">
            <label className="grid gap-2 text-sm font-semibold">Per run (USD)<input className={fieldClass} inputMode="decimal" onChange={(event) => setSingleRunLimit(event.target.value)} required value={singleRunLimit} /></label>
            <label className="grid gap-2 text-sm font-semibold">Monthly (USD)<input className={fieldClass} inputMode="decimal" onChange={(event) => setMonthlyLimit(event.target.value)} required value={monthlyLimit} /></label>
            <label className="grid gap-2 text-sm font-semibold">TTL<input className={`${fieldClass} font-mono`} defaultValue={template.spec.ttl} name="ttl" required /></label>
            <label className="grid gap-2 text-sm font-semibold">Runtime minutes<input className={`${fieldClass} font-mono`} inputMode="decimal" onChange={(event) => setRuntimeMinutesLimit(event.target.value)} placeholder="60" value={runtimeMinutesLimit} /></label>
          </div>
        </FormSection>
        <FormSection description="Only models supported by the inference gateway can be selected." title="Models">
          <TagSelect
            addPlaceholder="Add…"
            emptyPlaceholder="Search models…"
            inputDisabled={modelCatalog.length === 0}
            label="Models"
            onChange={(keys) => setModels(keys.flatMap((key) => {
              const model = modelCatalog.find((candidate) => modelKey(candidate) === key);
              return model ? [model] : [];
            }))}
            options={modelOptions}
            value={models.map(modelKey)}
          />
          {missingModels.length ? <p className="mt-2 text-sm text-warn">{missingModels.length} selected model{missingModels.length === 1 ? " is" : "s are"} not listed in the deployment capability catalog. Remove or replace before saving.</p> : modelCatalog.length === 0 ? <p className="mt-2 text-sm text-muted-ink">No models are listed in the deployment capability catalog.</p> : null}
        </FormSection>
        <FormSection description={`${capabilities.catalogs.map((catalog) => `${displayName(catalog.provider)} ${catalog.version}`).join(" · ") || "Tool catalog"}. Write and destructive tools are labelled.`} title="Tools">
          <div className="space-y-2">
            <TagSelect
              addPlaceholder="Add another tool…"
              emptyPlaceholder="Search GitHub tools…"
              inputDisabled={toolCatalog.length === 0}
              label="Tools"
              onChange={(keys) => setTools(keys.flatMap((key) => {
                const tool = toolCatalog.find((candidate) => toolKey(candidate) === key);
                return tool ? [{ provider: tool.provider, resource: tool.resource, action: tool.action }] : [];
              }))}
              options={toolOptions}
              value={tools.map(toolKey)}
            />
            {missingTools.length ? <p className="text-sm text-warn">{missingTools.length} selected tool{missingTools.length === 1 ? " is" : "s are"} not listed in the deployment capability catalog. Remove or replace before saving.</p> : toolCatalog.length === 0 ? <p className="text-sm text-muted-ink">No tools are listed in the deployment capability catalog.</p> : null}
            <p className="text-xs text-muted-ink">Type to filter. ↑ ↓ to move, Enter to add, Backspace removes the last tag.</p>
          </div>
        </FormSection>
        <FormSection description="Keep the default to auto-approve every valid request inside the ceiling, or provide a narrower complete envelope threshold." title="Auto-approval">
          <div className="space-y-4"><label className="flex min-h-10 items-center gap-3 text-sm font-semibold"><input checked={autoApproveToCeiling} onChange={(event) => setAutoApproveToCeiling(event.target.checked)} type="checkbox" />Auto-approve every request within the ceiling</label>{!autoApproveToCeiling ? <label className="grid gap-2 text-sm font-semibold">Auto-approve up to (complete envelope JSON)<textarea className="min-h-56 rounded-control border bg-canvas p-3 font-mono text-xs font-normal" onChange={(event) => setThresholdJson(event.target.value)} spellCheck={false} value={thresholdJson} /></label> : null}</div>
        </FormSection>
        <FormSection description="Optional. Leave resources blank to use platform defaults." title="Runner">
          <fieldset className="space-y-4"><legend className="sr-only">Runner</legend><div className="grid gap-3 sm:grid-cols-3">{(["linux", "mac", "windows"] as const).map((platform) => <label className="flex min-h-11 cursor-pointer items-center gap-2 rounded-control border px-4 text-sm capitalize has-[:checked]:border-brand has-[:checked]:bg-brand-soft" key={platform}><input defaultChecked={runner?.platforms?.includes(platform)} name="platforms" type="checkbox" value={platform} />{platform}</label>)}</div><div className="grid gap-4 sm:grid-cols-3"><label className="grid gap-2 text-sm font-semibold">Memory<input className={`${fieldClass} font-mono`} defaultValue={runner?.memory ?? ""} name="memory" placeholder="2Gi" /></label><label className="grid gap-2 text-sm font-semibold">Compute<input className={`${fieldClass} font-mono`} defaultValue={runner?.compute ?? ""} name="compute" placeholder="1000m" /></label><label className="grid gap-2 text-sm font-semibold">Storage<input className={`${fieldClass} font-mono`} defaultValue={runner?.storage ?? ""} name="storage" placeholder="10Gi" /></label></div></fieldset>
        </FormSection>
        {status !== "idle" && status !== "saving" ? <p className={`px-6 py-3 text-sm ${status === "saved" ? "text-ok" : "text-err"}`} role={status === "saved" ? "status" : "alert"}>{mutationMessage(status)}</p> : null}
        <footer className="sticky bottom-0 flex flex-wrap items-center justify-between gap-4 border-t bg-subtle px-6 py-4"><p className="text-sm text-muted-ink">{models.length} model{models.length === 1 ? "" : "s"} · {tools.length} tool{tools.length === 1 ? "" : "s"} · {singleRunLimit || "—"} USD per run</p><div className="flex gap-3"><Link className="rounded-control border bg-panel px-4 py-2 text-sm font-semibold" href="/admin/envelopes/templates">Cancel</Link><button className="rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={status === "saving"} name="action" type="submit" value="create">{status === "saving" ? "Saving…" : "Create template"}</button></div></footer>
      </form>
    );
  }
  return (
    <form className="space-y-6" onSubmit={submit}>
      <SectionCard>
      <div>
        <h2 className="text-xl font-semibold">{create ? "New template" : name}</h2>
        <p className="mt-1 text-sm text-muted-ink">{create ? "Initial revision 1" : `Current revision ${currentRevision}`}</p>
      </div>

      <FormSection description="The member roles allowed to request this template." title="Template identity and eligibility">
        <div className="grid gap-4 sm:grid-cols-2"><label className="grid gap-2 text-sm font-semibold">Display name<input className={fieldClass} name="displayName" onChange={(event) => setName(event.target.value)} required value={name} /></label><label className="grid gap-2 text-sm font-semibold">Eligible member roles<TagSelect addPlaceholder="Add another role…" allowCreate emptyPlaceholder="Add member role…" label="Eligible member roles" onChange={(next) => setRoles(next.join(", "))} options={roles.split(",").map((role) => role.trim()).filter(Boolean).map((role) => ({ key: role, kind: "neutral", label: role }))} value={roles.split(",").map((role) => role.trim()).filter(Boolean)} /></label></div>
      </FormSection>

      <FormSection description="Inference spend limits in USD and how long an envelope stays valid." title="Budget and lifetime">
        <div className="grid gap-4 sm:grid-cols-4"><label className="grid gap-2 text-sm font-semibold">Per run (USD)<input className={fieldClass} inputMode="decimal" onChange={(event) => setSingleRunLimit(event.target.value)} required value={singleRunLimit} /></label><label className="grid gap-2 text-sm font-semibold">Monthly (USD)<input className={fieldClass} inputMode="decimal" onChange={(event) => setMonthlyLimit(event.target.value)} required value={monthlyLimit} /></label><label className="grid gap-2 text-sm font-semibold">TTL<input className={`${fieldClass} font-mono`} defaultValue={template.spec.ttl} name="ttl" required /></label><label className="grid gap-2 text-sm font-semibold">Runtime minutes<input className={`${fieldClass} font-mono`} inputMode="decimal" onChange={(event) => setRuntimeMinutesLimit(event.target.value)} placeholder="60" value={runtimeMinutesLimit} /></label></div>
      </FormSection>

      <FormSection description="Only models supported by the inference gateway can be selected." title="Models"><fieldset><legend className="sr-only">Models</legend><TagSelect addPlaceholder="Add…" emptyPlaceholder="Search models…" inputDisabled={modelCatalog.length === 0} label="Models" onChange={(keys) => setModels(keys.flatMap((key) => { const model = [...modelCatalog, ...missingModels].find((candidate) => modelKey(candidate) === key); return model ? [model] : []; }))} options={modelOptions} value={models.map(modelKey)} />{missingModels.length ? <p className="mt-2 text-sm text-warn">{missingModels.length} selected model{missingModels.length === 1 ? " is" : "s are"} not listed in the deployment capability catalog. Remove or replace before saving.</p> : modelCatalog.length === 0 ? <p className="mt-2 text-sm text-muted-ink">No models are listed in the deployment capability catalog.</p> : null}</fieldset></FormSection>

      <FormSection description={`${capabilities.catalogs.map((catalog) => `${displayName(catalog.provider)} ${catalog.version}`).join(" · ") || "Tool catalog"}. Write and destructive tools are labelled.`} title="Tools"><fieldset><legend className="sr-only">Tools</legend><TagSelect addPlaceholder="Add another tool…" emptyPlaceholder="Search tools…" inputDisabled={toolCatalog.length === 0} label="Tools" onChange={(keys) => setTools(keys.flatMap((key) => { const tool = [...toolCatalog, ...missingTools].find((candidate) => toolKey(candidate) === key); return tool ? [{ provider: tool.provider, resource: tool.resource, action: tool.action }] : []; }))} options={toolOptions} value={tools.map(toolKey)} />{missingTools.length ? <p className="mt-2 text-sm text-warn">{missingTools.length} selected tool{missingTools.length === 1 ? " is" : "s are"} not listed in the deployment capability catalog. Remove or replace before saving.</p> : toolCatalog.length === 0 ? <p className="mt-2 text-sm text-muted-ink">No tools are listed in the deployment capability catalog.</p> : null}<p className="mt-2 text-xs text-muted-ink">Type to filter. ↑ ↓ to move, Enter to add, Backspace removes the last tag.</p></fieldset></FormSection>

      <FormSection description="Keep the default to auto-approve every valid request inside the ceiling, or provide a narrower complete envelope threshold." title="Auto-approval">
        <div className="space-y-4">
          <label className="flex min-h-10 items-center gap-3 text-sm font-semibold"><input checked={autoApproveToCeiling} onChange={(event) => setAutoApproveToCeiling(event.target.checked)} type="checkbox" />Auto-approve every request within the ceiling</label>
          {!autoApproveToCeiling ? <label className="grid gap-2 text-sm font-semibold">Auto-approve up to (complete envelope JSON)<textarea className="min-h-56 rounded-control border bg-canvas p-3 font-mono text-xs font-normal" onChange={(event) => setThresholdJson(event.target.value)} spellCheck={false} value={thresholdJson} /></label> : null}
        </div>
      </FormSection>

      <FormSection description="Optional. Leave resources blank to use platform defaults." title="Runner"><fieldset className="space-y-4"><legend className="sr-only">Runner</legend><div className="grid gap-3 sm:grid-cols-3">{(["linux", "mac", "windows"] as const).map((platform) => <label className="flex min-h-11 cursor-pointer items-center gap-2 rounded-control border px-4 text-sm capitalize has-[:checked]:border-brand has-[:checked]:bg-brand-soft" key={platform}><input defaultChecked={runner?.platforms?.includes(platform)} name="platforms" type="checkbox" value={platform} />{platform}</label>)}</div><div className="grid gap-4 sm:grid-cols-3"><label className="grid gap-2 text-sm font-semibold">Memory<input className={`${fieldClass} font-mono`} defaultValue={runner?.memory ?? ""} name="memory" placeholder="2Gi" /></label><label className="grid gap-2 text-sm font-semibold">Compute<input className={`${fieldClass} font-mono`} defaultValue={runner?.compute ?? ""} name="compute" placeholder="1000m" /></label><label className="grid gap-2 text-sm font-semibold">Storage<input className={`${fieldClass} font-mono`} defaultValue={runner?.storage ?? ""} name="storage" placeholder="10Gi" /></label></div></fieldset></FormSection>

      {status !== "idle" && status !== "saving" ? (
        <p className={status === "saved" ? "text-sm text-green-800" : "text-sm text-red-800"} role={status === "saved" ? "status" : "alert"}>{mutationMessage(status)}</p>
      ) : null}

      <div className="flex flex-wrap items-end gap-3 border-t pt-5">
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={status === "saving"} name="action" type="submit" value={create ? "create" : "version"}>{status === "saving" ? "Saving…" : create ? "Create template" : "Save new version"}</button>
        <label className="grid min-w-56 flex-1 gap-2 text-sm font-semibold">{create ? "Template ID" : "New template ID"}
          <input className={fieldClass} name="newTemplateId" placeholder="developer" required={create} />
        </label>
        {!create ? <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas disabled:opacity-50" disabled={status === "saving"} name="action" type="submit" value="copy">Save as new</button> : null}
      </div>
      </SectionCard>
    </form>
  );
}
