"use client";

import { useRouter, useSearchParams } from "next/navigation";
import { useCallback, useMemo, useState, type FormEvent } from "react";

import {
  listRequests,
  listTemplates,
  renderRepositoryBundleForEnvelope,
  submitBrowserRun,
  type BrowserPackageLocator,
  type EnvelopeRequestsResponse,
  type EnvelopeTemplatesResponse,
} from "@/api-client";
import { PageHeader, ResourceBoundary } from "@/components/workspace-ui";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";
import { listPublishedWorkflows, type PublishedWorkflowListResponse } from "@/workflows/api";

type RunNowData = {
  envelopes: EnvelopeRequestsResponse;
  templates: EnvelopeTemplatesResponse;
  workflows: PublishedWorkflowListResponse;
};

type SourceKind = "inline" | "repository" | "registry";

const fieldClass = "min-h-11 w-full rounded-control border bg-panel px-3 font-normal";

function inlineFiles(agentRef: string, prompt: string): Record<string, string> {
  return {
    "task-definition.json": JSON.stringify({
      schemaVersion: "steward.task-definition/v2",
      name: "browser-task",
      version: 1,
      runtime: { agentRef },
      prompt: "prompt.md",
      outputs: [{ path: "out", kind: "directory", required: true }],
    }, null, 2),
    "prompt.md": prompt,
  };
}

export function BrowserRunNowView() {
  const router = useRouter();
  const search = useSearchParams();
  const session = useSession();
  const load = useCallback(async () => {
    const [envelopes, templates, workflows] = await Promise.all([
      listRequests({ cache: "no-store", credentials: "same-origin" }),
      listTemplates({ cache: "no-store", credentials: "same-origin" }),
      listPublishedWorkflows(),
    ]);
    const response = !envelopes.response?.ok
      ? envelopes.response
      : !templates.response?.ok
        ? templates.response
        : workflows.response;
    return {
      data: envelopes.data && templates.data && workflows.data
        ? { envelopes: envelopes.data, templates: templates.data, workflows: workflows.data }
        : undefined,
      response,
    };
  }, []);
  const state = useApiResource<RunNowData>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Run an immutable inline, repository, or published package under one of your active Envelopes." title="Run now" />
      <ResourceBoundary state={state}>{(data) => <RunNowForm data={data} initialWorkflow={search.get("workflow")} onCreated={(taskUid) => router.push(`/runs/${taskUid}`)} session={session} />}</ResourceBoundary>
    </section>
  );
}

function RunNowForm({ data, initialWorkflow, onCreated, session }: Readonly<{
  data: RunNowData;
  initialWorkflow: string | null;
  onCreated: (taskUid: string) => void;
  session: ReturnType<typeof useSession>;
}>) {
  const active = data.envelopes.requests.filter((request) => request.status === "provisioned" && request.envelopeDigest);
  const initialKind: SourceKind = initialWorkflow ? "registry" : "inline";
  const [sourceKind, setSourceKind] = useState<SourceKind>(initialKind);
  const [envelopeId, setEnvelopeId] = useState(active[0]?.id ?? "");
  const [agentRef, setAgentRef] = useState(data.workflows.workflows[0]?.agent ?? "codex@0.140.0");
  const [prompt, setPrompt] = useState("Create $STEWARD_OUTPUT_DIR/out/hello.txt containing exactly the line: hello world. Use no tools and no network. Create no other files.");
  const [repository, setRepository] = useState("https://github.com/example-org/agentic-ops.git");
  const [revision, setRevision] = useState("git:ref:main");
  const [path, setPath] = useState("catalog/hello/task-definition.json");
  const defaultWorkflow = data.workflows.workflows.find((workflow) => `${workflow.name}@${workflow.version}` === initialWorkflow) ?? data.workflows.workflows[0];
  const [workflowRef, setWorkflowRef] = useState(defaultWorkflow ? `${defaultWorkflow.name}@${defaultWorkflow.version}` : "");
  const [inputs, setInputs] = useState("{}");
  const [status, setStatus] = useState<"idle" | "submitting" | "copying" | "error">("idle");
  const selectedEnvelope = active.find((request) => request.id === envelopeId);
  const selectedTemplate = data.templates.templates.find((template) => template.id === selectedEnvelope?.templateId && template.revision === selectedEnvelope?.templateRevision);
  const inlineAllowed = selectedTemplate?.allowInlineBrowserTasks !== false;
  const files = useMemo(() => inlineFiles(agentRef, prompt), [agentRef, prompt]);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated" || !selectedEnvelope?.envelopeDigest) return;
    let parsedInputs: unknown;
    try {
      parsedInputs = JSON.parse(inputs);
    } catch {
      setStatus("error");
      return;
    }
    let packageLocator: BrowserPackageLocator;
    if (sourceKind === "inline") {
      if (!inlineAllowed) {
        setStatus("error");
        return;
      }
      packageLocator = { source: "inline", path: "task-definition.json", files };
    } else if (sourceKind === "repository") {
      packageLocator = { source: repository.trim(), revision: revision.trim(), path: path.trim() };
    } else {
      const [name, version] = workflowRef.split("@");
      packageLocator = { source: `steward:registry/${name}`, revision: `steward:version:${version}`, path: "task-definition.json" };
    }
    const envelopeDigest = selectedEnvelope.envelopeDigest.startsWith("steward:")
      ? selectedEnvelope.envelopeDigest
      : `steward:${selectedEnvelope.envelopeDigest}`;
    setStatus("submitting");
    const result = await submitBrowserRun({
      body: { package: packageLocator, envelopeDigest, inputs: parsedInputs },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "Idempotency-Key": crypto.randomUUID(), "X-Steward-CSRF": session.value.csrf },
    });
    if (result.data && result.response?.ok) onCreated(result.data.taskUid);
    else setStatus("error");
  }

  async function copyPackage() {
    if (session.status !== "authenticated" || !selectedEnvelope) return;
    setStatus("copying");
    try {
      const result = await renderRepositoryBundleForEnvelope({
        body: {
          repository: repository.trim(),
          packagePath: "task-definition.json",
          invocationPath: ".steward/invocations/browser-task.json",
        },
        credentials: "same-origin",
        headers: { "X-Steward-CSRF": session.value.csrf },
        path: { request_id: selectedEnvelope.id },
      });
      if (!result.data || !result.response?.ok) {
        setStatus("error");
        return;
      }
      await navigator.clipboard.writeText(JSON.stringify({ ...files, ...result.data.files }, null, 2));
      setStatus("idle");
    } catch {
      setStatus("error");
    }
  }

  if (active.length === 0) return <p className="rounded-card border bg-panel p-5 text-sm text-muted-ink">Provision an Envelope before starting a browser run.</p>;
  return <form className="space-y-6 rounded-card border bg-panel p-6" onSubmit={(event) => void submit(event)}>
    <div className="grid gap-4 md:grid-cols-2">
      <label className="grid gap-2 text-sm font-semibold">Envelope<select className={fieldClass} onChange={(event) => setEnvelopeId(event.target.value)} value={envelopeId}>{active.map((request) => <option key={request.id} value={request.id}>{request.templateId ?? "Custom"} · rev {request.approvedEnvelope?.revision ?? request.requestedEnvelope.revision}</option>)}</select></label>
      <label className="grid gap-2 text-sm font-semibold">Package source<select className={fieldClass} onChange={(event) => setSourceKind(event.target.value as SourceKind)} value={sourceKind}><option disabled={!inlineAllowed} value="inline">Inline package{inlineAllowed ? "" : " (disabled by template)"}</option><option value="repository">Git repository</option><option value="registry">Published workflow</option></select></label>
    </div>
    {sourceKind === "inline" ? <div className="grid gap-4"><label className="grid gap-2 text-sm font-semibold">Coding agent<input className={`${fieldClass} font-mono`} onChange={(event) => setAgentRef(event.target.value)} value={agentRef} /></label><label className="grid gap-2 text-sm font-semibold">Prompt<textarea className="min-h-40 rounded-control border bg-panel p-3 font-normal" onChange={(event) => setPrompt(event.target.value)} value={prompt} /></label><label className="grid gap-2 text-sm font-semibold">Save target repository<input className={`${fieldClass} font-mono`} onChange={(event) => setRepository(event.target.value)} value={repository} /></label><div><button className="w-fit rounded-control border px-4 py-2 text-sm font-semibold" onClick={() => void copyPackage()} type="button">{status === "copying" ? "Rendering…" : "Copy repository bundle"}</button><p className="mt-2 text-xs text-muted-ink">Copies a JSON file map containing the exact package, a git:trigger invocation, and caller workflow pinned to the reviewed steward-run release.</p></div></div> : null}
    {sourceKind === "repository" ? <div className="grid gap-4"><label className="grid gap-2 text-sm font-semibold">Repository<input className={`${fieldClass} font-mono`} onChange={(event) => setRepository(event.target.value)} value={repository} /></label><div className="grid gap-4 md:grid-cols-2"><label className="grid gap-2 text-sm font-semibold">Revision<input className={`${fieldClass} font-mono`} onChange={(event) => setRevision(event.target.value)} value={revision} /></label><label className="grid gap-2 text-sm font-semibold">Package path<input className={`${fieldClass} font-mono`} onChange={(event) => setPath(event.target.value)} value={path} /></label></div><p className="text-xs text-muted-ink">Branch and tag refs are resolved server-side and recorded as an exact commit before execution.</p></div> : null}
    {sourceKind === "registry" ? <label className="grid gap-2 text-sm font-semibold">Published workflow<select className={fieldClass} onChange={(event) => setWorkflowRef(event.target.value)} value={workflowRef}>{data.workflows.workflows.map((workflow) => <option key={`${workflow.name}@${workflow.version}`} value={`${workflow.name}@${workflow.version}`}>{workflow.displayName} · {workflow.name}@{workflow.version}</option>)}</select></label> : null}
    <label className="grid gap-2 text-sm font-semibold">Inputs (JSON object)<textarea className="min-h-28 rounded-control border bg-panel p-3 font-mono text-xs font-normal" onChange={(event) => setInputs(event.target.value)} spellCheck={false} value={inputs} /></label>
    {status === "error" ? <p className="text-sm text-err" role="alert">The run request was rejected. Check the package locator, inputs, and Envelope authority.</p> : null}
    <div className="flex justify-end"><button className="rounded-control bg-brand px-5 py-2.5 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={status === "submitting" || (sourceKind === "inline" && !inlineAllowed)} type="submit">{status === "submitting" ? "Starting…" : "Run now"}</button></div>
  </form>;
}
