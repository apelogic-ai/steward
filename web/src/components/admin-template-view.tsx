"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useState, type FormEvent } from "react";

import {
  authorAdminEnvelopeTemplate,
  getAdminEnvelopeTemplate,
  type BrowserEnvelope,
  type BrowserEnvelopeTemplateResponse,
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
const supportedModel = "openai/gpt-5.4";
const supportedTool = "github:repository:get_file_contents";
const supportedToolProvider = "github";
const supportedToolAction = "repository:get_file_contents";
const modelCatalog = [
  { value: supportedModel, enabled: true },
  { value: "openai/gpt-5.3", enabled: false },
  { value: "anthropic/claude-opus-4", enabled: false },
  { value: "google/gemini-2.5-pro", enabled: false },
] as const;
const toolProviderCatalog = [
  { label: "GitHub", value: supportedToolProvider, enabled: true },
  { label: "GitLab", value: "gitlab", enabled: false },
  { label: "Jira", value: "jira", enabled: false },
] as const;
const githubToolCatalog = [
  { value: supportedToolAction, enabled: true },
  { value: "repository:list_issues", enabled: false },
  { value: "repository:create_issue", enabled: false },
  { value: "pull_request:get", enabled: false },
] as const;

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

function toolValue(tool: ToolGrant): string {
  return `${tool.provider}:${tool.resource}:${tool.action}`;
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
      <PageHeader description="Review the current immutable envelope templates available in Steward." title="Envelope templates" />
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
  const normalizedTemplate = state.status === "ready"
    ? normalizeEnvelopeTemplateResponse(state.value, memberRole)
    : null;
  const acceptedState = state.status === "ready"
    ? normalizedTemplate
      ? { status: "ready" as const, value: normalizedTemplate }
      : { status: "error" as const }
    : state;

  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader
        actions={<Link className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas" href="/admin/envelopes/templates">All templates</Link>}
        description="Inspect the current immutable revision and author a successor."
        title="Envelope template"
      />
      <ResourceBoundary state={acceptedState}>{({ envelope }) => (
        <TemplateEditor csrf={csrf} key={`${memberRole}:${envelope.revision}`} memberRole={memberRole} template={envelope} />
      )}</ResourceBoundary>
    </section>
  );
}

function TemplateEditor({ csrf, memberRole, template }: Readonly<{ csrf: string; memberRole: string; template: BrowserEnvelope }>) {
  const router = useRouter();
  const [status, setStatus] = useState<TemplateMutationState>("idle");
  const [currentRevision, setCurrentRevision] = useState(template.revision);
  const [models, setModels] = useState<Array<ModelRef>>(template.spec.llms);
  const [modelInput, setModelInput] = useState(supportedModel);
  const [tools, setTools] = useState<Array<ToolGrant>>(template.spec.tools);
  const [toolProviderInput, setToolProviderInput] = useState(supportedToolProvider);
  const [toolInput, setToolInput] = useState(supportedToolAction);
  const [limitType, setLimitType] = useState<LimitType>("singleRun");
  const [monthlyLimit, setMonthlyLimit] = useState(template.spec.budget.monthlyLimit);
  const [singleRunLimit, setSingleRunLimit] = useState(template.spec.budget.singleRunLimit ?? "");
  const limitAmount = limitType === "singleRun" ? singleRunLimit : monthlyLimit;

  function addModel() {
    if (modelInput !== supportedModel || models.some((model) => modelValue(model) === supportedModel)) return;
    setModels([...models, { provider: "openai", model: "gpt-5.4" }]);
  }

  function addTool() {
    if (`${toolProviderInput}:${toolInput}` !== supportedTool
      || tools.some((tool) => toolValue(tool) === supportedTool)) return;
    setTools([...tools, { provider: "github", resource: "repository", action: "get_file_contents" }]);
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const fields = new FormData(event.currentTarget);
    const submitter = (event.nativeEvent as SubmitEvent).submitter;
    const action = submitter instanceof HTMLButtonElement ? submitter.value : "version";
    const saveAsNew = action === "copy";
    const newTemplateId = String(fields.get("newTemplateId") ?? "").trim();
    const targetRole = saveAsNew ? newTemplateId : memberRole;
    const platforms = fields.getAll("platforms").filter((value): value is RunnerPlatform =>
      value === "linux" || value === "mac" || value === "windows");
    if (!targetRole
      || !monthlyLimit.trim()
      || !singleRunLimit.trim()
      || models.length === 0
      || models.some((model) => modelValue(model) !== supportedModel)
      || tools.some((tool) => toolValue(tool) !== supportedTool)) {
      setStatus("rejected");
      return;
    }
    const memory = String(fields.get("memory") ?? "").trim();
    const compute = String(fields.get("compute") ?? "").trim();
    const storage = String(fields.get("storage") ?? "").trim();
    const body: BrowserEnvelope = {
      revision: saveAsNew ? 1 : currentRevision + 1,
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
      if (saveAsNew) router.push(`/admin/envelopes/templates/${encodeURIComponent(targetRole)}`);
      return;
    }
    setStatus(classifyMutationFailure(result.response?.status));
  }

  const runner = template.spec.runner;
  return (
    <form className="space-y-6 rounded-panel border bg-panel p-6 shadow-sm" onSubmit={submit}>
      <div>
        <h2 className="text-xl font-semibold">{displayName(memberRole)}</h2>
        <p className="mt-1 text-sm text-muted-ink">Current revision {currentRevision}</p>
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
            <select className={fieldClass} onChange={(event) => setModelInput(event.target.value)} value={modelInput}>
              {modelCatalog.map((model) => <option disabled={!model.enabled} key={model.value} value={model.value}>{model.value}</option>)}
            </select>
          </label>
          <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-50" disabled={models.some((model) => modelValue(model) === modelInput)} onClick={addModel} type="button">Add model</button>
        </div>
        <ul className="flex flex-wrap gap-2" role="list">
          {models.map((model) => {
            const value = modelValue(model);
            return <li className={value === supportedModel ? "rounded-full border px-3 py-1.5 text-sm" : "rounded-full border px-3 py-1.5 text-sm text-muted-ink opacity-60"} key={value}>{value}</li>;
          })}
        </ul>
      </fieldset>

      <fieldset className="space-y-3">
        <legend className="text-base font-semibold">Tools</legend>
        <div className="grid gap-3 sm:grid-cols-[1fr_2fr_auto] sm:items-end">
          <label className="grid gap-2 text-sm font-semibold">Tool provider
            <select className={fieldClass} onChange={(event) => setToolProviderInput(event.target.value)} value={toolProviderInput}>
              {toolProviderCatalog.map((provider) => <option disabled={!provider.enabled} key={provider.value} value={provider.value}>{provider.label}</option>)}
            </select>
          </label>
          <label className="grid gap-2 text-sm font-semibold">Tool
            <select className={fieldClass} onChange={(event) => setToolInput(event.target.value)} value={toolInput}>
              {githubToolCatalog.map((tool) => <option disabled={!tool.enabled} key={tool.value} value={tool.value}>{tool.value}</option>)}
            </select>
          </label>
          <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-50" disabled={tools.some((tool) => toolValue(tool) === `${toolProviderInput}:${toolInput}`)} onClick={addTool} type="button">Add tool</button>
        </div>
        <ul className="flex flex-wrap gap-2" role="list">
          {tools.map((tool) => <li className="rounded-full border px-3 py-1.5 text-sm" key={toolValue(tool)}>{toolValue(tool)}</li>)}
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
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50" disabled={status === "saving"} name="action" type="submit" value="version">{status === "saving" ? "Saving…" : "Save new version"}</button>
        <label className="grid min-w-56 flex-1 gap-2 text-sm font-semibold">New template ID
          <input className={fieldClass} name="newTemplateId" placeholder="reviewer" />
        </label>
        <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas disabled:opacity-50" disabled={status === "saving"} name="action" type="submit" value="copy">Save as new</button>
      </div>
    </form>
  );
}
