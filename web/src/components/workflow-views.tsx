"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useState, type FormEvent } from "react";

import { DefinitionList, EmptyState, PageHeader, PrimaryLink, ResourceBoundary } from "@/components/workspace-ui";
import { classifyMutationFailure, type MutationFailureState } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";
import {
  getAdminWorkflow,
  listAdminWorkflows,
  publishWorkflow,
  publishWorkflowVersion,
  type WorkflowListResponse,
  type WorkflowRevision,
  type WorkflowRevisionResponse,
} from "@/workflows/api";
import { workflowReference } from "@/workflows/contracts";

const SUPPORTED_AGENT = "codex@0.117.0";

export function AdminWorkflowsView() {
  const load = useCallback(() => listAdminWorkflows(), []);
  const state = useApiResource<WorkflowListResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader actions={<PrimaryLink href="/admin/workflows/new">Create workflow</PrimaryLink>} description="Publish immutable, versioned agent definitions." title="Workflows" />
      <ResourceBoundary state={state}>{({ workflows }) => workflows.length === 0 ? (
        <EmptyState title="No data" />
      ) : (
        <ul className="grid gap-4 lg:grid-cols-2">
          {workflows.map((workflow) => (
            <li className="rounded-panel border bg-panel p-5 shadow-sm" key={workflowReference(workflow)}>
              <div className="flex items-start justify-between gap-4">
                <div><h2 className="font-semibold">{workflow.displayName}</h2><p className="mt-1 font-mono text-xs text-muted-ink">{workflowReference(workflow)}</p></div>
                <span className="rounded-full border border-blue-500/40 bg-blue-500/10 px-3 py-1 text-xs font-semibold text-blue-700 dark:text-blue-300">Published</span>
              </div>
              <p className="mt-4 text-sm text-muted-ink">{workflow.agent}</p>
              <Link className="mt-5 inline-flex min-h-11 items-center text-sm font-semibold text-brand hover:text-brand-strong" href={`/admin/workflows/${workflow.name}/versions/${workflow.version}`}>View version →</Link>
            </li>
          ))}
        </ul>
      )}</ResourceBoundary>
    </section>
  );
}

export function AdminWorkflowDetailView({ name, version }: Readonly<{ name: string; version: number }>) {
  const load = useCallback(() => getAdminWorkflow(name, version), [name, version]);
  const state = useApiResource<WorkflowRevisionResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Inspect an immutable published Workflow revision." title="Workflow" />
      <ResourceBoundary state={state}>{({ workflow }) => <WorkflowDetail workflow={workflow} />}</ResourceBoundary>
    </section>
  );
}

function WorkflowDetail({ workflow }: Readonly<{ workflow: WorkflowRevision }>) {
  return (
    <article className="space-y-6 rounded-panel border bg-panel p-6 shadow-sm">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div><h2 className="text-xl font-semibold">{workflow.displayName}</h2><p className="mt-1 font-mono text-xs text-muted-ink">{workflowReference(workflow)}</p></div>
        <PrimaryLink href={`/admin/workflows/${workflow.name}/new-version`}>Create new version</PrimaryLink>
      </div>
      <DefinitionList items={[["Agent", workflow.agent], ["Content digest", workflow.contentDigest], ["Published", workflow.publishedAt]]} />
      <div><h3 className="text-sm font-semibold">Prompt</h3><pre className="mt-2 whitespace-pre-wrap rounded-md border bg-canvas p-4 text-sm">{workflow.prompt}</pre></div>
    </article>
  );
}

export function NewWorkflowView({ from }: Readonly<{ from?: WorkflowRevision }>) {
  const router = useRouter();
  const session = useSession();
  const [submission, setSubmission] = useState<"idle" | "submitting" | MutationFailureState>("idle");
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated") return;
    setSubmission("submitting");
    const data = new FormData(event.currentTarget);
    const content = {
      agent: SUPPORTED_AGENT,
      displayName: String(data.get("displayName")),
      prompt: String(data.get("prompt")),
    };
    const result = from
      ? await publishWorkflowVersion(session.value.csrf, from.name, content)
      : await publishWorkflow(session.value.csrf, { ...content, name: String(data.get("name")) });
    if (result.data && result.response?.status === 201) {
      router.push(`/admin/workflows/${result.data.workflow.name}/versions/${result.data.workflow.version}`);
      return;
    }
    setSubmission(classifyMutationFailure(result.response?.status ?? 0));
  }
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description={from ? "Publish the next immutable revision. The existing version will not change." : "Publish an immutable agent definition."} title={from ? `New ${from.name} version` : "New workflow"} />
      <form className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm" onSubmit={submit}>
        <label className="grid gap-2 text-sm font-semibold">Name<input className="min-h-11 rounded-md border px-3 font-normal" defaultValue={from?.name ?? ""} disabled={Boolean(from)} name="name" pattern="[a-z](?:[a-z0-9]|-)*[a-z0-9]" required /></label>
        <label className="grid gap-2 text-sm font-semibold">Display name<input className="min-h-11 rounded-md border px-3 font-normal" defaultValue={from?.displayName ?? ""} name="displayName" required /></label>
        <label className="grid gap-2 text-sm font-semibold">Agent<select className="min-h-11 rounded-md border bg-panel px-3 font-normal" defaultValue={SUPPORTED_AGENT} name="agent"><option value={SUPPORTED_AGENT}>{SUPPORTED_AGENT}</option></select></label>
        <label className="grid gap-2 text-sm font-semibold">Prompt<textarea className="min-h-52 rounded-md border p-3 font-normal" defaultValue={from?.prompt ?? ""} name="prompt" required /></label>
        {submission !== "idle" && submission !== "submitting" ? <p className="text-sm text-red-800" role="alert">{{ conflict: "This Workflow name or version was published concurrently.", rejected: "The Workflow content was rejected.", forbidden: "The authorization boundary rejected publication.", unavailable: "The Workflow service is unavailable.", error: "The Workflow could not be published." }[submission]}</p> : null}
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50" disabled={submission === "submitting"} type="submit">{submission === "submitting" ? "Publishing…" : from ? "Publish new version" : "Publish workflow"}</button>
      </form>
    </section>
  );
}

export function NewWorkflowVersionView({ name }: Readonly<{ name: string }>) {
  const list = useCallback(() => listAdminWorkflows(), []);
  const state = useApiResource<WorkflowListResponse>(list);
  return <ResourceBoundary state={state}>{({ workflows }) => {
    const current = workflows.find((workflow) => workflow.name === name);
    return current ? <NewWorkflowView from={current} /> : <EmptyState title="No data" />;
  }}</ResourceBoundary>;
}
