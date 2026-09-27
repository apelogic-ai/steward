"use client";

import Link from "next/link";
import { useCallback, useRef, useState, type FormEvent } from "react";

import {
  listAdminEnvelopeTemplates,
  provisionAdminEnvelope,
  type BrowserEnvelopeTemplateListItem,
} from "@/api-client";
import { DefinitionList, EmptyState, PageHeader, ResourceBoundary } from "@/components/workspace-ui";
import { classifyMutationFailure, type MutationFailureState } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

type ProvisionOptions = {
  templates: Array<BrowserEnvelopeTemplateListItem>;
};

async function loadProvisionOptions(): Promise<{ data?: ProvisionOptions; response?: Response }> {
  const templates = await listAdminEnvelopeTemplates({ cache: "no-store", credentials: "same-origin" });
  if (!templates.data || !templates.response?.ok) return { response: templates.response };
  return {
    data: { templates: templates.data.templates },
    response: templates.response,
  };
}

export function AdminEnvelopeProvisionView() {
  const session = useSession();
  const load = useCallback(() => loadProvisionOptions(), []);
  const state = useApiResource<ProvisionOptions>(load);

  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader
        actions={<Link className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas" href="/admin/envelopes/templates">View templates</Link>}
        description="Provision an exact template revision directly to an existing canonical user."
        title="Provision envelope"
      />
      {session.status !== "authenticated" ? (
        <EmptyState title="Session unavailable"><p>The authoritative administrator session is not available.</p></EmptyState>
      ) : (
        <ResourceBoundary state={state}>{(options) => options.templates.length === 0 ? (
          <EmptyState title="No data" />
        ) : <ProvisionForm csrf={session.value.csrf} options={options} />}</ResourceBoundary>
      )}
    </section>
  );
}

function ProvisionForm({ csrf, options }: Readonly<{ csrf: string; options: ProvisionOptions }>) {
  const [ownerUserId, setOwnerUserId] = useState("");
  const [templateId, setTemplateId] = useState(options.templates[0].id);
  const [status, setStatus] = useState<"idle" | "submitting" | "provisioned" | MutationFailureState>("idle");
  const [provisionedId, setProvisionedId] = useState<string | null>(null);
  const idempotencyKey = useRef<string | null>(null);
  const template = options.templates.find((item) => item.id === templateId) ?? options.templates[0];

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setStatus("submitting");
    const result = await provisionAdminEnvelope({
      body: {
        idempotencyKey: idempotencyKey.current ??= crypto.randomUUID(),
        ownerUserId,
        requestedEnvelope: template.envelope,
        templateId: template.id,
        templateRevision: template.envelope.revision,
      },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": csrf },
    });
    if (result.data && result.response?.ok) {
      setProvisionedId(result.data.request.envelopeInstanceId ?? result.data.request.requestId);
      setStatus("provisioned");
      return;
    }
    setStatus(classifyMutationFailure(result.response?.status));
  }

  const failure = status !== "idle" && status !== "submitting" && status !== "provisioned" ? {
    conflict: "The selected envelope conflicts with the user's active authority. Reload before retrying.",
    rejected: "The user is not eligible for this template or the requested envelope is invalid.",
    forbidden: "The Rust authorization boundary rejected this provisioning operation.",
    unavailable: "The authoritative envelope service is unavailable.",
    error: "The user or exact template revision could not be provisioned.",
  }[status] : null;

  return (
    <form className="space-y-6 rounded-panel border bg-panel p-6 shadow-sm" onSubmit={submit}>
      <div className="grid gap-5 md:grid-cols-2">
        <label className="grid gap-2 text-sm font-semibold">Canonical user ID
          <input className="min-h-11 min-w-0 rounded-md border bg-panel px-3 font-mono font-normal" disabled={status === "submitting"} onChange={(event) => { setOwnerUserId(event.target.value); idempotencyKey.current = null; setStatus("idle"); }} placeholder="usr_…" required value={ownerUserId} />
        </label>
        <label className="grid gap-2 text-sm font-semibold">Template
          <select className="min-h-11 min-w-0 rounded-md border bg-panel px-3 font-normal" disabled={status === "submitting"} onChange={(event) => { setTemplateId(event.target.value); idempotencyKey.current = null; setStatus("idle"); }} value={template.id}>
            {options.templates.map((item) => <option key={item.id} value={item.id}>{item.displayName} · revision {item.envelope.revision}</option>)}
          </select>
        </label>
      </div>
      <section aria-labelledby="provision-summary-title" className="space-y-3 rounded-md border bg-canvas p-4">
        <h2 className="font-semibold" id="provision-summary-title">Authority to provision</h2>
        <DefinitionList items={[
          ["Eligible roles", template.memberRoles.join(", ")],
          ["Monthly limit", `${template.envelope.spec.budget.monthlyLimit} ${template.envelope.spec.budget.currency}`],
          ["Runtime minutes", template.envelope.spec.runtimeMinutesLimit ? `${template.envelope.spec.runtimeMinutesLimit} min / month` : "Unlimited"],
          ["TTL", template.envelope.spec.ttl],
          ["Models", template.envelope.spec.llms.length.toString()],
          ["Tools", template.envelope.spec.tools.length.toString()],
        ]} />
      </section>
      <p className="text-sm text-muted-ink">The selected user remains the immutable owner. Your canonical administrator identity is recorded as the actor.</p>
      {status === "provisioned" ? <p className="rounded-md bg-notice p-4 text-sm" role="status"><strong>Envelope provisioned.</strong>{provisionedId ? ` Instance ${provisionedId}.` : ""}</p> : null}
      {failure ? <p className="text-sm text-red-800" role="alert">{failure}</p> : null}
      <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:cursor-not-allowed disabled:opacity-50" disabled={status === "submitting"} type="submit">{status === "submitting" ? "Provisioning…" : "Provision envelope"}</button>
    </form>
  );
}
