"use client";

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

const fieldClass = "min-h-11 min-w-0 w-full rounded-md border bg-panel px-3 font-normal";

function starterTemplate(): BrowserEnvelope {
  return {
    revision: 1,
    spec: {
      budget: { currency: "USD", monthlyLimit: "0.00" },
      llms: [],
      tools: [],
      ttl: "1h",
      runner: { platforms: [] },
    },
  };
}

function parseModels(value: string): Array<ModelRef> | null {
  const models: Array<ModelRef> = [];
  for (const line of value.split("\n").map((item) => item.trim()).filter(Boolean)) {
    const separator = line.indexOf("/");
    if (separator <= 0 || separator === line.length - 1) return null;
    models.push({ provider: line.slice(0, separator), model: line.slice(separator + 1) });
  }
  return models;
}

function parseTools(value: string): Array<ToolGrant> | null {
  const tools: Array<ToolGrant> = [];
  for (const line of value.split("\n").map((item) => item.trim()).filter(Boolean)) {
    const [provider, resource, action, ...extra] = line.split(":");
    if (!provider || !resource || !action || extra.length > 0) return null;
    tools.push({ provider, resource, action });
  }
  return tools;
}

function mutationMessage(status: Exclude<TemplateMutationState, "idle" | "saving">): string {
  return {
    saved: "Template revision accepted by the Rust authority.",
    conflict: "A newer template revision already exists. Load it before authoring another revision.",
    rejected: "The member role or envelope fields are invalid, so no authority was changed.",
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
  const initialRole = session.value.memberRoles[0] ?? "developer";
  return <AuthenticatedTemplateView csrf={session.value.csrf} initialRole={initialRole} />;
}

function AuthenticatedTemplateView({ csrf, initialRole }: Readonly<{ csrf: string; initialRole: string }>) {
  const [roleInput, setRoleInput] = useState(initialRole);
  const [selectedRole, setSelectedRole] = useState(initialRole);
  const load = useCallback(() => getAdminEnvelopeTemplate({
    cache: "no-store",
    credentials: "same-origin",
    path: { member_role: selectedRole },
  }), [selectedRole]);
  const state = useApiResource<BrowserEnvelopeTemplateResponse>(load);

  function selectRole(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const normalized = roleInput.trim();
    if (normalized) setSelectedRole(normalized);
  }

  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Read and author versioned member-role ceilings through Steward's existing admission ledger." eyebrow="Administration" title="Envelope templates" />
      <form className="flex flex-wrap items-end gap-3 rounded-panel border bg-panel p-5 shadow-sm" onSubmit={selectRole}>
        <label className="grid min-w-56 flex-1 gap-2 text-sm font-semibold">Template role
          <input className={fieldClass} onChange={(event) => setRoleInput(event.target.value)} required value={roleInput} />
        </label>
        <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas" type="submit">Load template</button>
      </form>
      {state.status === "ready" ? (
        <TemplateEditor csrf={csrf} key={`${selectedRole}:${state.value.envelope.revision}`} memberRole={selectedRole} template={state.value.envelope} />
      ) : state.status === "not-found" ? (
        <div className="space-y-4">
          <EmptyState title="No template found"><p>Author revision 1 to create the first template for this member role.</p></EmptyState>
          <TemplateEditor csrf={csrf} key={`${selectedRole}:new`} memberRole={selectedRole} template={starterTemplate()} />
        </div>
      ) : (
        <ResourceBoundary state={state}>{() => null}</ResourceBoundary>
      )}
    </section>
  );
}

function TemplateEditor({ csrf, memberRole, template }: Readonly<{ csrf: string; memberRole: string; template: BrowserEnvelope }>) {
  const [status, setStatus] = useState<TemplateMutationState>("idle");
  const [currentRevision, setCurrentRevision] = useState(template.revision);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const fields = new FormData(event.currentTarget);
    const revision = Number(fields.get("revision"));
    const models = parseModels(String(fields.get("models") ?? ""));
    const tools = parseTools(String(fields.get("tools") ?? ""));
    const platforms = fields.getAll("platforms").filter((value): value is RunnerPlatform =>
      value === "linux" || value === "mac" || value === "windows");
    if (!Number.isSafeInteger(revision) || revision <= 0 || !models || !tools) {
      setStatus("rejected");
      return;
    }
    const memory = String(fields.get("memory") ?? "").trim();
    const compute = String(fields.get("compute") ?? "").trim();
    const storage = String(fields.get("storage") ?? "").trim();
    const body: BrowserEnvelope = {
      revision,
      spec: {
        budget: {
          currency: String(fields.get("currency") ?? "").trim(),
          monthlyLimit: String(fields.get("monthlyLimit") ?? "").trim(),
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
      path: { member_role: memberRole },
    });
    if (result.data && result.response?.status === 201) {
      setCurrentRevision(result.data.envelope.revision);
      setStatus("saved");
      return;
    }
    setStatus(classifyMutationFailure(result.response?.status));
  }

  const runner = template.spec.runner;
  return (
    <form className="space-y-6 rounded-panel border bg-panel p-6 shadow-sm" onSubmit={submit}>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div><h2 className="text-xl font-semibold">{memberRole}</h2><p className="mt-1 text-sm text-muted-ink">Current revision {currentRevision}</p></div>
        <label className="grid w-32 gap-2 text-sm font-semibold">Revision
          <input className={fieldClass} defaultValue={template.revision} min="1" name="revision" required type="number" />
        </label>
      </div>
      <fieldset className="grid gap-4 sm:grid-cols-3">
        <legend className="mb-3 text-base font-semibold">Budget and lifetime</legend>
        <label className="grid gap-2 text-sm font-semibold">Currency
          <input className={fieldClass} defaultValue={template.spec.budget.currency} maxLength={3} name="currency" required />
        </label>
        <label className="grid gap-2 text-sm font-semibold">Monthly limit
          <input className={fieldClass} defaultValue={template.spec.budget.monthlyLimit} inputMode="decimal" name="monthlyLimit" required />
        </label>
        <label className="grid gap-2 text-sm font-semibold">Time to live
          <input className={fieldClass} defaultValue={template.spec.ttl} name="ttl" required />
        </label>
      </fieldset>
      <div className="grid gap-4 lg:grid-cols-2">
        <label className="grid gap-2 text-sm font-semibold">Models
          <span className="font-normal text-muted-ink">One provider/model entry per line.</span>
          <textarea className="min-h-36 min-w-0 w-full rounded-md border bg-panel p-3 font-mono text-sm font-normal" defaultValue={template.spec.llms.map((model) => `${model.provider}/${model.model}`).join("\n")} name="models" />
        </label>
        <label className="grid gap-2 text-sm font-semibold">Tools
          <span className="font-normal text-muted-ink">One provider:resource:action entry per line.</span>
          <textarea className="min-h-36 min-w-0 w-full rounded-md border bg-panel p-3 font-mono text-sm font-normal" defaultValue={template.spec.tools.map((tool) => `${tool.provider}:${tool.resource}:${tool.action}`).join("\n")} name="tools" />
        </label>
      </div>
      <fieldset className="space-y-4">
        <legend className="text-base font-semibold">Runner ceiling</legend>
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
      {status !== "idle" && status !== "saving" ? (
        <p className={status === "saved" ? "text-sm text-green-800" : "text-sm text-red-800"} role={status === "saved" ? "status" : "alert"}>{mutationMessage(status)}</p>
      ) : null}
      <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50" disabled={status === "saving"} type="submit">{status === "saving" ? "Authoring…" : "Author template revision"}</button>
    </form>
  );
}
