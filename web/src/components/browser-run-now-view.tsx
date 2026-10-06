"use client";

import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { useCallback, useState, type FormEvent } from "react";

import {
  listRequests,
  listTemplates,
  myTask,
  myTasks,
  submitBrowserRun,
  type BrowserPackageLocator,
  type BrowserEnvelope,
  type BrowserTaskView,
  type BrowserTasksResponse,
  type ExecutionBindingAdvertisement,
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
  tasks: BrowserTasksResponse;
  task: BrowserTaskView | null;
};

type SourceKind = "inline" | "repository" | "registry";

const fieldClass = "min-h-11 w-full rounded-control border bg-panel px-3 font-normal";
const genericRunNowFailure = "The run request was rejected. Check the package locator, inputs, and Envelope authority.";

type CompatibleAgent = {
  agentRef: string;
  model: { provider: string; model: string } | null;
  compatible: boolean;
  reason: string | null;
};

type EffectiveAgentSelection = {
  selected: CompatibleAgent | undefined;
  warning: string | null;
};

type ExactTaskLocator = Pick<BrowserTaskView, "files" | "path" | "revision" | "source">;

export function packageLocatorForTask(task: ExactTaskLocator): BrowserPackageLocator {
  return {
    source: task.source,
    revision: task.revision,
    path: task.path,
    ...(task.source === "inline" ? { files: task.files } : {}),
  };
}

function agentModelProvider(agentRef: string): { provider: string; label: string } | null {
  if (agentRef.startsWith("codex@")) return { provider: "openai", label: "OpenAI" };
  if (agentRef.startsWith("claude-code@")) return { provider: "anthropic", label: "Anthropic" };
  return null;
}

export function compatibleAgents(agentRefs: string[], models: Array<{ provider: string; model: string }>): CompatibleAgent[] {
  return agentRefs.map((agentRef) => {
    const required = agentModelProvider(agentRef);
    if (!required) {
      return { agentRef, model: null, compatible: false, reason: "Steward does not know this agent's required model family." };
    }
    const model = models.find((candidate) => candidate.provider === required.provider) ?? null;
    return model
      ? { agentRef, model, compatible: true, reason: null }
      : { agentRef, model: null, compatible: false, reason: `Requires an ${required.label} model, which this Envelope does not allow.` };
  });
}

export function effectiveAgentSelection(
  agentOptions: CompatibleAgent[],
  requestedAgentRef: string,
): EffectiveAgentSelection {
  const requested = agentOptions.find((agent) => agent.agentRef === requestedAgentRef);
  const selected = requested?.compatible
    ? requested
    : agentOptions.find((agent) => agent.compatible);
  return {
    selected,
    warning: selected ? null : requested?.reason ?? "No compatible coding agent is available for this Envelope.",
  };
}

function failureHint(code: string | null, reason: string): string | null {
  if (code === "task.direct_package_source_disabled") return "Choose an inline task or ask an administrator to enable GitHub source packages.";
  if (reason.includes("selected model is not allowed")) return "Choose an agent compatible with this Envelope, or request an Envelope that allows its model.";
  if (reason.includes("requirements exceed the selected Envelope")) return "Choose a package within this Envelope's approved models, tools, budget, and runner limits.";
  if (reason.includes("repository") && reason.includes("allowed")) return "Use a repository in the operator's allowed source list.";
  return null;
}

export function runNowFailureMessage(error: unknown): string {
  if (!error || typeof error !== "object") return genericRunNowFailure;
  const value = error as Record<string, unknown>;
  const code = typeof value.error === "string" && value.error.trim() ? value.error : null;
  const reason = typeof value.failureReason === "string" && value.failureReason.trim()
    ? value.failureReason
    : typeof value.message === "string" && value.message.trim()
      ? value.message
      : null;
  if (reason) {
    const hint = failureHint(code, reason);
    return `${code ? `${code}: ` : ""}${reason}${hint ? ` ${hint}` : ""}`;
  }
  if (code) return `Run request failed (${code}).`;
  return genericRunNowFailure;
}

export function inlineFiles(
  agentRef: string,
  prompt: string,
  envelope: BrowserEnvelope,
  model: { provider: string; model: string },
): Record<string, string> {
  return {
    "task-definition.json": JSON.stringify({
      schemaVersion: "steward.task-definition/v2",
      name: "browser-task",
      version: 1,
      runtime: { agentRef, model },
      promptText: prompt,
      outputs: [{ path: "out", kind: "directory", required: true }],
      requires: {
        authority: {
          llms: [model],
          tools: envelope.spec.tools,
          budget: {
            monthlyLimit: envelope.spec.budget.monthlyLimit,
            singleRunLimit: envelope.spec.budget.singleRunLimit ?? null,
            currency: envelope.spec.budget.currency,
          },
          ttl: envelope.spec.ttl,
          runner: {
            platforms: envelope.spec.runner?.platforms ?? [],
            memory: envelope.spec.runner?.memory ?? null,
            compute: envelope.spec.runner?.compute ?? null,
            storage: envelope.spec.runner?.storage ?? null,
          },
        },
      },
    }, null, 2),
  };
}

export function BrowserRunNowView() {
  const router = useRouter();
  const search = useSearchParams();
  const taskDigest = search.get("task");
  const session = useSession();
  const load = useCallback(async () => {
    const [envelopes, templates, workflows, tasks, task] = await Promise.all([
      listRequests({ cache: "no-store", credentials: "same-origin" }),
      listTemplates({ cache: "no-store", credentials: "same-origin" }),
      listPublishedWorkflows(),
      myTasks({ cache: "no-store", credentials: "same-origin" }),
      taskDigest ? myTask({ cache: "no-store", credentials: "same-origin", path: { content_digest: taskDigest } }) : Promise.resolve(null),
    ]);
    const response = !envelopes.response?.ok
      ? envelopes.response
      : !templates.response?.ok
        ? templates.response
        : !workflows.response?.ok
          ? workflows.response
          : !tasks.response?.ok
            ? tasks.response
          : task && !task.response?.ok
            ? task.response
            : workflows.response;
    return {
      data: envelopes.data && templates.data && workflows.data && tasks.data && (!taskDigest || task?.data)
        ? { envelopes: envelopes.data, templates: templates.data, workflows: workflows.data, tasks: tasks.data, task: task?.data?.task ?? null }
        : undefined,
      response,
    };
  }, [taskDigest]);
  const state = useApiResource<RunNowData>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Run an immutable inline, repository, or published package under one of your active Envelopes." title="Run now" />
      <ResourceBoundary state={state}>{(data) => data.task
        ? <RunNowForm data={data} initialWorkflow={search.get("workflow")} onCreated={(taskUid) => router.push(`/runs/${taskUid}`)} session={session} />
        : <TaskPicker data={data} initialWorkflow={search.get("workflow")} onSelect={(digest) => router.replace(`/runs/new?task=${encodeURIComponent(digest)}`)} />}</ResourceBoundary>
    </section>
  );
}

function TaskPicker({ data, initialWorkflow, onSelect }: Readonly<{
  data: RunNowData;
  initialWorkflow: string | null;
  onSelect: (digest: string) => void;
}>) {
  const requestedWorkflow = initialWorkflow
    ? (() => {
        const separator = initialWorkflow.lastIndexOf("@");
        if (separator <= 0 || separator === initialWorkflow.length - 1) return null;
        const name = initialWorkflow.slice(0, separator);
        const version = initialWorkflow.slice(separator + 1);
        return data.tasks.tasks.find((task) => task.source === `steward:registry/${name}` && task.revision === `steward:version:${version}`) ?? null;
      })()
    : null;
  const [digest, setDigest] = useState(requestedWorkflow?.contentDigest ?? data.tasks.tasks[0]?.contentDigest ?? "");
  if (data.tasks.tasks.length === 0) return <div className="rounded-card border bg-panel p-6"><p className="text-sm text-muted-ink">Create and review a Task before starting a Run.</p><Link className="mt-4 inline-flex rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand" href="/tasks/new">Create Task</Link></div>;
  return <div className="space-y-5 rounded-card border bg-panel p-6">
    <label className="grid gap-2 text-sm font-semibold">Task<select className={fieldClass} onChange={(event) => setDigest(event.target.value)} value={digest}>{data.tasks.tasks.map((task) => <option key={`${task.contentDigest}-${task.source}`} value={task.contentDigest}>{task.name}@{task.version} · {task.source === "inline" ? "draft" : task.source.startsWith("steward:registry/") ? "published" : "Git"}</option>)}</select></label>
    <p className="text-sm text-muted-ink">Choose one exact immutable Task version. You can review its package before selecting an Envelope and running it.</p>
    <div className="flex flex-wrap justify-end gap-2"><Link className="inline-flex min-h-10 items-center rounded-control border px-4 text-sm font-semibold" href="/tasks/new">Create Task</Link><Link className="inline-flex min-h-10 items-center rounded-control border px-4 text-sm font-semibold" href={`/tasks/${encodeURIComponent(digest)}`}>Review</Link><button className="rounded-control bg-brand px-5 py-2.5 text-sm font-semibold text-on-brand" onClick={() => onSelect(digest)} type="button">Continue</button></div>
  </div>;
}

function RunNowForm({ data, initialWorkflow, onCreated, session }: Readonly<{
  data: RunNowData;
  initialWorkflow: string | null;
  onCreated: (taskUid: string) => void;
  session: ReturnType<typeof useSession>;
}>) {
  const active = data.envelopes.requests.filter((request) => request.status === "provisioned" && request.envelopeDigest);
  const exactTask = data.task;
  const initialKind: SourceKind = exactTask ? "inline" : initialWorkflow ? "registry" : "inline";
  const [sourceKind, setSourceKind] = useState<SourceKind>(initialKind);
  const [envelopeId, setEnvelopeId] = useState(active[0]?.id ?? "");
  const [agentRef, setAgentRef] = useState(data.workflows.agents[0]?.agentRef ?? "");
  const [prompt, setPrompt] = useState("Create $STEWARD_OUTPUT_DIR/out/hello.txt containing exactly the line: hello world. Use no tools and no network. Create no other files.");
  const [repository, setRepository] = useState("https://github.com/example-org/agentic-ops.git");
  const [revision, setRevision] = useState("git:ref:main");
  const [path, setPath] = useState("catalog/hello/task-definition.json");
  const defaultWorkflow = data.workflows.workflows.find((workflow) => `${workflow.name}@${workflow.version}` === initialWorkflow) ?? data.workflows.workflows[0];
  const [workflowRef, setWorkflowRef] = useState(defaultWorkflow ? `${defaultWorkflow.name}@${defaultWorkflow.version}` : "");
  const [inputs, setInputs] = useState("{}");
  const [captureExecutionLog, setCaptureExecutionLog] = useState(false);
  const [status, setStatus] = useState<"idle" | "submitting" | "error">("idle");
  const [failure, setFailure] = useState<string | null>(null);
  const selectedEnvelope = active.find((request) => request.id === envelopeId);
  const selectedTemplate = data.templates.templates.find((template) => template.id === selectedEnvelope?.templateId && template.revision === selectedEnvelope?.templateRevision);
  const inlineAllowed = selectedTemplate?.allowInlineBrowserTasks !== false;
  const envelope = selectedEnvelope?.approvedEnvelope ?? selectedEnvelope?.requestedEnvelope;
  const agentOptions = compatibleAgents(
    data.workflows.agents.map((agent: ExecutionBindingAdvertisement) => agent.agentRef),
    envelope?.spec.llms ?? [],
  );
  const { selected: selectedAgent, warning: agentWarning } = effectiveAgentSelection(agentOptions, agentRef);
  const effectiveAgentRef = selectedAgent?.agentRef ?? "";
  const selectedModel = selectedAgent?.compatible ? selectedAgent.model : null;
  const files = envelope && selectedModel
    ? inlineFiles(effectiveAgentRef, prompt, envelope, selectedModel)
    : null;

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated" || !selectedEnvelope?.envelopeDigest) return;
    let parsedInputs: unknown;
    try {
      parsedInputs = JSON.parse(inputs);
    } catch {
      setFailure("Inputs must be valid JSON.");
      setStatus("error");
      return;
    }
    let packageLocator: BrowserPackageLocator;
    if (exactTask) {
      packageLocator = packageLocatorForTask(exactTask);
    } else if (sourceKind === "inline") {
      if (!inlineAllowed) {
        setFailure("Inline packages are disabled by the selected Envelope template.");
        setStatus("error");
        return;
      }
      if (!files || !selectedModel) {
        setFailure("No compatible coding agent is available for the selected Envelope.");
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
    setFailure(null);
    setStatus("submitting");
    const result = await submitBrowserRun({
      body: {
        package: packageLocator,
        envelopeDigest,
        inputs: parsedInputs,
        diagnostics: { executionLog: captureExecutionLog ? "full" : "off" },
      },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "Idempotency-Key": crypto.randomUUID(), "X-Steward-CSRF": session.value.csrf },
    });
    if (result.data && result.response?.ok) {
      onCreated(result.data.taskUid);
      return;
    }
    setFailure(runNowFailureMessage(result.error));
    setStatus("error");
  }

  if (active.length === 0) return <p className="rounded-card border bg-panel p-5 text-sm text-muted-ink">Provision an Envelope before starting a browser run.</p>;
  return <form className="space-y-6 rounded-card border bg-panel p-6" onSubmit={(event) => void submit(event)}>
    <div className="grid gap-4 md:grid-cols-2">
      <label className="grid gap-2 text-sm font-semibold">Envelope<select className={fieldClass} onChange={(event) => setEnvelopeId(event.target.value)} value={envelopeId}>{active.map((request) => <option key={request.id} value={request.id}>{request.templateId ?? "Custom"} · rev {request.approvedEnvelope?.revision ?? request.requestedEnvelope.revision}</option>)}</select></label>
      {exactTask ? <div className="grid gap-2 text-sm font-semibold">Task<div className={`${fieldClass} flex items-center font-mono text-xs`}>{exactTask.name}@{exactTask.version} · {exactTask.contentDigest}</div></div> : <label className="grid gap-2 text-sm font-semibold">Package source<select className={fieldClass} onChange={(event) => setSourceKind(event.target.value as SourceKind)} value={sourceKind}><option disabled={!inlineAllowed} value="inline">Inline package{inlineAllowed ? "" : " (disabled by template)"}</option><option value="repository">Git repository</option><option value="registry">Published workflow</option></select></label>}
    </div>
    {exactTask ? <p className="text-sm text-muted-ink">The exact immutable package is locked. Choose an Envelope and optional inputs for this run.</p> : null}
    {!exactTask && sourceKind === "inline" ? <div className="grid gap-4"><label className="grid gap-2 text-sm font-semibold">Coding agent<select className={`${fieldClass} font-mono`} onChange={(event) => setAgentRef(event.target.value)} value={effectiveAgentRef}>{agentOptions.map((agent) => <option disabled={!agent.compatible} key={agent.agentRef} value={agent.agentRef}>{agent.agentRef}{agent.compatible ? ` · ${agent.model?.provider}/${agent.model?.model}` : ` · ${agent.reason}`}</option>)}</select></label>{agentWarning ? <p className="text-sm text-warn" role="status">{agentWarning}</p> : null}<label className="grid gap-2 text-sm font-semibold">Prompt<span className="text-xs font-normal text-muted-ink">Write results under <code>$STEWARD_OUTPUT_DIR/out/</code>. Only those files are collected; a run with no <code>out/</code> file fails.</span><textarea className="min-h-40 rounded-control border bg-panel p-3 font-normal" onChange={(event) => setPrompt(event.target.value)} value={prompt} /></label><p className="text-xs text-muted-ink">This inline task uses one compatible model and inherits the selected Envelope&apos;s approved tools, budget, TTL, and runner limits. Open the Task from the completed Run to reuse or publish the exact package.</p></div> : null}
    {!exactTask && sourceKind === "repository" ? <div className="grid gap-4"><label className="grid gap-2 text-sm font-semibold">Repository<input className={`${fieldClass} font-mono`} onChange={(event) => setRepository(event.target.value)} value={repository} /></label><div className="grid gap-4 md:grid-cols-2"><label className="grid gap-2 text-sm font-semibold">Ref or immutable commit<input className={`${fieldClass} font-mono`} onChange={(event) => setRevision(event.target.value)} value={revision} /></label><label className="grid gap-2 text-sm font-semibold">Package path<input className={`${fieldClass} font-mono`} onChange={(event) => setPath(event.target.value)} value={path} /></label></div><p className="text-xs text-muted-ink">The repository must be in the operator&apos;s allowed source list. Steward resolves a ref to an exact commit and validates the package&apos;s declared requirements before execution.</p></div> : null}
    {!exactTask && sourceKind === "registry" ? <label className="grid gap-2 text-sm font-semibold">Published workflow<select className={fieldClass} onChange={(event) => setWorkflowRef(event.target.value)} value={workflowRef}>{data.workflows.workflows.map((workflow) => <option key={`${workflow.name}@${workflow.version}`} value={`${workflow.name}@${workflow.version}`}>{workflow.displayName} · {workflow.name}@{workflow.version}</option>)}</select></label> : null}
    <label className="grid gap-2 text-sm font-semibold">Inputs (JSON object)<span className="text-xs font-normal text-muted-ink">Optional JSON (up to 16 KiB), available to the agent as <code>in/inputs.json</code>. Reference it in your prompt; it cannot change the agent, model, tools, or Envelope. Example: <code>{'{"release":"v1.2.3"}'}</code>.</span><textarea className="min-h-28 rounded-control border bg-panel p-3 font-mono text-xs font-normal" onChange={(event) => setInputs(event.target.value)} spellCheck={false} value={inputs} /></label>
    <label className="flex items-start gap-3 rounded-control border border-warn/30 bg-warn-soft p-4 text-sm"><input checked={captureExecutionLog} className="mt-1 size-4" onChange={(event) => setCaptureExecutionLog(event.target.checked)} type="checkbox" /><span><strong className="block font-semibold text-warn">Capture execution log</strong><span className="mt-1 block text-muted-ink">Retain stdout and stderr for this run. Execution logs may reproduce arbitrary user, tool, or agent output.</span></span></label>
    {status === "error" ? <p className="text-sm text-err" role="alert">{failure ?? genericRunNowFailure}</p> : null}
    <div className="flex justify-end"><button className="rounded-control bg-brand px-5 py-2.5 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={status === "submitting" || (!exactTask && sourceKind === "inline" && (!inlineAllowed || !selectedModel))} type="submit">{status === "submitting" ? "Starting…" : "Run now"}</button></div>
  </form>;
}
