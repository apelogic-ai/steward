"use client";

import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { useCallback, useState, type FormEvent } from "react";

import {
  getStarterTask,
  getUserCapabilities,
  listRequests,
  listTemplates,
  myTask,
  myTasks,
  submitBrowserRun,
  type BrowserPackageLocator,
  type BrowserTaskView,
  type BrowserTasksResponse,
  type DirectTaskDefinition,
  type EnvelopeRequestsResponse,
  type EnvelopeTemplatesResponse,
  type GithubRepositoryView,
  type StarterTaskSetting,
  type UserCapabilitiesResponse,
} from "@/api-client";
import { DiagnosticDetails, type DiagnosticItem } from "@/components/diagnostic-details";
import {
  compatibleAgents,
  effectiveAgentSelection,
  effectiveModelSelection,
  ModelPicker,
  preferredTaskModel,
  type Model,
} from "@/components/model-selection";
import { canonicalWorkspaceRef, workspaceRepositoryName } from "@/components/workspace-repository-controls";
import { PageHeader, ResourceBoundary } from "@/components/workspace-ui";
import { RepositoryResource, useGithubRepositories } from "@/data/github-repositories";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";
import { listPublishedWorkflows, type PublishedWorkflowListResponse } from "@/workflows/api";

type RunNowData = {
  capabilities: UserCapabilitiesResponse;
  envelopes: EnvelopeRequestsResponse;
  templates: EnvelopeTemplatesResponse;
  workflows: PublishedWorkflowListResponse;
  tasks: BrowserTasksResponse;
  task: BrowserTaskView | null;
  starterTask: StarterTaskSetting;
};

type SourceKind = "inline" | "repository" | "registry";

const fieldClass = "min-h-11 w-full rounded-control border bg-panel px-3 font-normal";
const genericRunNowFailure = "The run request was rejected. Check the package, inputs, and selected Envelope limits, then try again.";

type ExactTaskLocator = Pick<BrowserTaskView, "files" | "path" | "revision" | "source">;

export function packageLocatorForTask(task: ExactTaskLocator): BrowserPackageLocator {
  return {
    source: task.source,
    revision: task.revision,
    path: task.path,
    ...(task.source === "inline" ? { files: task.files } : {}),
  };
}

function failureHint(code: string | null, reason: string): string | null {
  if (code === "task.direct_package_source_disabled") return "Choose an inline task or ask an administrator to enable GitHub source packages.";
  if (reason.includes("selected model is not allowed")) return "Choose an agent compatible with this Envelope, or request an Envelope that allows its model.";
  return null;
}

function repositoryWorkspaceFailure(reason: string): boolean {
  const normalized = reason.toLocaleLowerCase("en-US").replaceAll(/[_-]+/g, " ");
  const unavailable = ["not found", "not approved", "not admitted", "not allowed", "unavailable", "unresolved"]
    .some((phrase) => normalized.includes(phrase));
  return (normalized.includes("repository") && unavailable)
    || ((normalized.includes("branch") || normalized.includes("tag")) && normalized.includes("not found"));
}

function sentence(value: string): string {
  const trimmed = value.trim();
  return /[.!?]$/.test(trimmed) ? trimmed : `${trimmed}.`;
}

export function runNowFailureMessage(error: unknown, workspaceRepository?: string): string {
  if (!error || typeof error !== "object") return genericRunNowFailure;
  const value = error as Record<string, unknown>;
  const code = typeof value.error === "string" && value.error.trim() ? value.error : null;
  const reason = typeof value.failureReason === "string" && value.failureReason.trim()
    ? value.failureReason
    : null;
  if (reason) {
    const hint = failureHint(code, reason);
    if (hint) return `The run request was rejected. ${hint}`;
    const repository = workspaceRepository && repositoryWorkspaceFailure(reason)
      ? ` Repository: ${workspaceRepository}.`
      : "";
    return `The run request was rejected. ${sentence(reason)}${repository}`;
  }
  return genericRunNowFailure;
}

function runNowDiagnostics(error: unknown): DiagnosticItem[] {
  if (!error || typeof error !== "object") return [];
  const value = error as Record<string, unknown>;
  return [
    { label: "Code", value: value.error },
    { label: "Reason", value: value.failureReason ?? value.message },
  ];
}

export function inlineFiles(
  starterTask: StarterTaskSetting,
  agentRef: string,
  model: { provider: string; model: string },
  checkout?: ({
    repository: Pick<GithubRepositoryView, "name" | "owner">;
    ref: string;
  } | {
    repository: Pick<GithubRepositoryView, "ownerId" | "repositoryId">;
  }) & { historyDepth?: number },
): Record<string, string> {
  const taskDefinition: DirectTaskDefinition = {
    ...starterTask.taskDefinition,
    runtime: { ...starterTask.taskDefinition.runtime, agentRef, model },
    ...(checkout
      ? {
          workspace: [{
            git: {
              // TODO(#397): remove these casts when the generated schema includes named repository and ref selectors.
              repository: ("ref" in checkout
                ? workspaceRepositoryName(checkout.repository)
                : {
                    ownerId: checkout.repository.ownerId,
                    repositoryId: checkout.repository.repositoryId,
                  }) as never,
              ref: ("ref" in checkout ? canonicalWorkspaceRef(checkout.ref) : "trigger") as never,
              ...(checkout.historyDepth ? { history: { depth: checkout.historyDepth } } : {}),
            },
          }] as unknown as DirectTaskDefinition["workspace"],
        }
      : {}),
    ...(starterTask.taskDefinition.requires
      ? {
          requires: {
            ...starterTask.taskDefinition.requires,
            authority: {
              ...starterTask.taskDefinition.requires.authority,
              llms: [model],
            },
          },
        }
      : {}),
  };
  return {
    [starterTask.packagePath]: JSON.stringify(taskDefinition, null, 2),
  };
}

export function BrowserRunNowView() {
  const router = useRouter();
  const search = useSearchParams();
  const taskDigest = search.get("task");
  const session = useSession();
  const load = useCallback(async () => {
    const [capabilities, envelopes, templates, workflows, tasks, task, starterTask] = await Promise.all([
      getUserCapabilities({ cache: "no-store", credentials: "same-origin" }),
      listRequests({ cache: "no-store", credentials: "same-origin" }),
      listTemplates({ cache: "no-store", credentials: "same-origin" }),
      listPublishedWorkflows(),
      myTasks({ cache: "no-store", credentials: "same-origin" }),
      taskDigest ? myTask({ cache: "no-store", credentials: "same-origin", path: { content_digest: taskDigest } }) : Promise.resolve(null),
      getStarterTask({ cache: "no-store", credentials: "same-origin" }),
    ]);
    const response = !capabilities.response?.ok
      ? capabilities.response
      : !envelopes.response?.ok
      ? envelopes.response
      : !templates.response?.ok
        ? templates.response
        : !workflows.response?.ok
          ? workflows.response
          : !tasks.response?.ok
          ? tasks.response
          : task && !task.response?.ok
            ? task.response
            : !starterTask.response?.ok
              ? starterTask.response
              : workflows.response;
    return {
      data: capabilities.data && envelopes.data && templates.data && workflows.data && tasks.data && starterTask.data && (!taskDigest || task?.data)
        ? { capabilities: capabilities.data, envelopes: envelopes.data, templates: templates.data, workflows: workflows.data, tasks: tasks.data, task: task?.data?.task ?? null, starterTask: starterTask.data.starterTask }
        : undefined,
      response,
    };
  }, [taskDigest]);
  const state = useApiResource<RunNowData>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Run an immutable inline, repository, or published package under one of your active Envelopes." title="Run now" />
      <ResourceBoundary state={state}>{(data) => !data.task && search.get("choose") === "task"
        ? <TaskPicker data={data} initialWorkflow={search.get("workflow")} onSelect={(digest) => router.replace(`/runs/new?task=${encodeURIComponent(digest)}`)} />
        : <RunNowForm data={data} initialWorkflow={search.get("workflow")} key={data.task?.contentDigest ?? "starter"} onCreated={(taskUid) => router.push(`/runs/${taskUid}`)} session={session} />}</ResourceBoundary>
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
  const { refresh: refreshRepositories, state: repositoryState } = useGithubRepositories();
  const active = data.envelopes.requests.filter((request) => request.status === "provisioned" && request.envelopeDigest);
  const exactTask = data.task;
  const starterTask = data.starterTask;
  const initialKind: SourceKind = exactTask ? "inline" : initialWorkflow ? "registry" : "inline";
  const [sourceKind, setSourceKind] = useState<SourceKind>(initialKind);
  const [envelopeId, setEnvelopeId] = useState(active[0]?.id ?? "");
  const [agentRef, setAgentRef] = useState(starterTask.taskDefinition.runtime.agentRef);
  const [model, setModel] = useState<Model | null>(() => preferredTaskModel(starterTask.taskDefinition));
  const [prompt, setPrompt] = useState(starterTask.taskDefinition.promptText ?? "");
  const [repository, setRepository] = useState(starterTask.git?.repository ?? "");
  const [revision, setRevision] = useState(starterTask.git?.revision ?? "");
  const [path, setPath] = useState(starterTask.git?.path ?? "");
  const configuredWorkflow = starterTask.publishedWorkflow ?? null;
  const defaultWorkflow = data.workflows.workflows.find((workflow) => `${workflow.name}@${workflow.version}` === initialWorkflow)
    ?? data.workflows.workflows.find((workflow) => `${workflow.name}@${workflow.version}` === configuredWorkflow)
    ?? data.workflows.workflows[0];
  const [workflowRef, setWorkflowRef] = useState(defaultWorkflow ? `${defaultWorkflow.name}@${defaultWorkflow.version}` : "");
  const [inputs, setInputs] = useState(exactTask ? "{}" : JSON.stringify(starterTask.inputs, null, 2));
  const [captureExecutionLog, setCaptureExecutionLog] = useState(!exactTask && starterTask.executionLog === "full");
  const [checkoutRepository, setCheckoutRepository] = useState(false);
  const [workspaceRepositoryId, setWorkspaceRepositoryId] = useState("");
  const [workspaceRef, setWorkspaceRef] = useState("");
  const [historyDepth, setHistoryDepth] = useState("");
  const [status, setStatus] = useState<"idle" | "submitting" | "error">("idle");
  const [failure, setFailure] = useState<string | null>(null);
  const [failureDetails, setFailureDetails] = useState<DiagnosticItem[]>([]);
  const selectedEnvelope = active.find((request) => request.id === envelopeId);
  const selectedTemplate = data.templates.templates.find((template) => template.id === selectedEnvelope?.templateId && template.revision === selectedEnvelope?.templateRevision);
  const inlineAllowed = selectedTemplate?.allowInlineBrowserTasks !== false;
  const envelope = selectedEnvelope?.approvedEnvelope ?? selectedEnvelope?.requestedEnvelope;
  const agentOptions = compatibleAgents(
    data.capabilities.agents,
    envelope?.spec.llms ?? [],
  );
  const { selected: selectedAgent, warning: agentWarning } = effectiveAgentSelection(agentOptions, agentRef);
  const effectiveAgentRef = selectedAgent?.agentRef ?? "";
  const { selected: selectedModel, warning: modelWarning } = effectiveModelSelection(
    model,
    selectedAgent,
  );
  const workspaceRepositories = repositoryState.status === "ready" ? repositoryState.value.repositories : [];
  const defaultWorkspaceRepository = workspaceRepositories.find((candidate) => candidate.ready);
  const selectedWorkspaceRepository = workspaceRepositories.find((candidate) => candidate.repositoryId === workspaceRepositoryId);
  const effectiveWorkspaceRef = workspaceRef || selectedWorkspaceRepository?.defaultBranch || "";
  const workspaceRefValid = !selectedWorkspaceRepository || effectiveWorkspaceRef.trim().length > 0;
  const parsedHistoryDepth = historyDepth.trim() === "" ? undefined : Number(historyDepth);
  const historyDepthValid = parsedHistoryDepth === undefined || (Number.isSafeInteger(parsedHistoryDepth) && parsedHistoryDepth > 0);
  const workspaceCheckout = checkoutRepository && historyDepthValid
    ? selectedWorkspaceRepository?.ready && workspaceRefValid
      ? { repository: selectedWorkspaceRepository, ref: effectiveWorkspaceRef, historyDepth: parsedHistoryDepth }
      : defaultWorkspaceRepository?.ready
        ? { repository: defaultWorkspaceRepository, historyDepth: parsedHistoryDepth }
        : undefined
    : undefined;
  const workspaceCheckoutReady = !checkoutRepository || Boolean(workspaceCheckout);
  const failureWorkspaceRepository = selectedWorkspaceRepository ?? defaultWorkspaceRepository;
  const files = selectedModel
    ? inlineFiles(
        { ...starterTask, taskDefinition: { ...starterTask.taskDefinition, promptText: prompt } },
        effectiveAgentRef,
        selectedModel,
        workspaceCheckout,
      )
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
      packageLocator = { source: "inline", path: starterTask.packagePath, files };
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
    setFailureDetails([]);
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
    setFailure(runNowFailureMessage(
      result.error,
      checkoutRepository && failureWorkspaceRepository
        ? workspaceRepositoryName(failureWorkspaceRepository)
        : undefined,
    ));
    setFailureDetails(runNowDiagnostics(result.error));
    setStatus("error");
  }

  if (active.length === 0) return <p className="rounded-card border bg-panel p-5 text-sm text-muted-ink">Provision an Envelope before starting a browser run.</p>;
  return <form className="space-y-6 rounded-card border bg-panel p-6" onSubmit={(event) => void submit(event)}>
    {!exactTask ? <div className="flex flex-wrap items-start justify-between gap-4"><div><h2 className="font-semibold">{starterTask.title ?? starterTask.taskDefinition.name}</h2>{starterTask.description ? <p className="mt-1 text-sm text-muted-ink">{starterTask.description}</p> : null}</div><Link className="rounded-control border px-4 py-2 text-sm font-semibold" href="/runs/new?choose=task">Choose saved Task</Link></div> : null}
    <div className="grid gap-4 md:grid-cols-2">
      <label className="grid gap-2 text-sm font-semibold">Envelope<select className={fieldClass} onChange={(event) => setEnvelopeId(event.target.value)} value={envelopeId}>{active.map((request) => <option key={request.id} value={request.id}>{request.templateId ?? "Custom"} · rev {request.approvedEnvelope?.revision ?? request.requestedEnvelope.revision}</option>)}</select></label>
      {exactTask ? <div className="grid gap-2 text-sm font-semibold">Task<div className={`${fieldClass} flex items-center font-mono text-xs`}>{exactTask.name}@{exactTask.version} · {exactTask.contentDigest}</div></div> : <label className="grid gap-2 text-sm font-semibold">Package source<select className={fieldClass} onChange={(event) => setSourceKind(event.target.value as SourceKind)} value={sourceKind}><option disabled={!inlineAllowed} value="inline">Inline package{inlineAllowed ? "" : " (disabled by template)"}</option><option value="repository">Git repository</option><option value="registry">Published workflow</option></select></label>}
    </div>
    {exactTask ? <p className="text-sm text-muted-ink">The exact immutable package is locked. Choose an Envelope and optional inputs for this run.</p> : null}
    {!exactTask && sourceKind === "inline" ? <div className="grid gap-4"><div className="grid gap-4 md:grid-cols-2"><label className="grid gap-2 text-sm font-semibold">Coding agent<select className={`${fieldClass} font-mono`} onChange={(event) => setAgentRef(event.target.value)} value={effectiveAgentRef}>{agentOptions.map((agent) => <option disabled={!agent.compatible} key={agent.agentRef} value={agent.agentRef}>{agent.displayName ?? agent.agentRef} · {agent.agentRef}{agent.compatible ? "" : ` · ${agent.reason}`}</option>)}</select></label><ModelPicker className={fieldClass} models={selectedAgent?.models ?? []} onChange={setModel} selected={selectedModel} /></div>{agentWarning ?? modelWarning ? <p className="text-sm text-warn" role="status">{agentWarning ?? modelWarning}</p> : null}<label className="grid gap-2 text-sm font-semibold">Prompt<span className="text-xs font-normal text-muted-ink">Write results under <code>$STEWARD_OUTPUT_DIR/out/</code>. Only those files are collected; a run with no <code>out/</code> file fails.</span><textarea className="min-h-40 rounded-control border bg-panel p-3 font-normal" onChange={(event) => setPrompt(event.target.value)} value={prompt} /></label><p className="text-xs text-muted-ink">This inline task uses the selected compatible model and {starterTask.taskDefinition.requires ? "uses its saved access requirements" : "inherits the selected Envelope's approved tools, budget, time-to-live limit, and runner limits"}. Open the Task from the completed Run to reuse or publish the exact package.</p></div> : null}
    {!exactTask && sourceKind === "inline" ? <div className="space-y-3 rounded-control border p-4"><label className="flex items-center gap-3 text-sm font-semibold"><input checked={checkoutRepository} onChange={(event) => setCheckoutRepository(event.target.checked)} type="checkbox" />Check out this repository</label>{checkoutRepository ? <RepositoryResource onRetry={refreshRepositories} state={repositoryState}>{({ repositories }) => <div className="grid gap-4 md:grid-cols-3"><label className="grid gap-2 text-sm font-semibold">Workspace repository<select className={fieldClass} onChange={(event) => { const nextId = event.target.value; const next = repositories.find((candidate) => candidate.repositoryId === nextId); setWorkspaceRepositoryId(nextId); setWorkspaceRef(next?.defaultBranch ?? ""); }} value={workspaceRepositoryId}><option value="">Default repository at trigger revision</option>{repositories.map((candidate) => <option disabled={!candidate.ready} key={candidate.repositoryId} value={candidate.repositoryId}>{workspaceRepositoryName(candidate)}{candidate.ready ? "" : " · not approved"}</option>)}</select></label><label className="grid gap-2 text-sm font-semibold">Branch or tag<input className={`${fieldClass} font-mono`} disabled={!selectedWorkspaceRepository} onChange={(event) => setWorkspaceRef(event.target.value)} placeholder={selectedWorkspaceRepository?.defaultBranch ?? "Choose a named repository first"} value={effectiveWorkspaceRef} /></label><label className="grid gap-2 text-sm font-semibold">History depth (optional)<input className={fieldClass} min={1} onChange={(event) => setHistoryDepth(event.target.value)} placeholder="Files only" step={1} type="number" value={historyDepth} /></label><p className="text-xs text-muted-ink md:col-span-3">Choose a named repository to pin a branch or tag. The default keeps the repository and revision supplied by the trigger.</p></div>}</RepositoryResource> : null}{checkoutRepository && !workspaceRefValid ? <p className="text-sm text-err" role="alert">Enter a branch or tag.</p> : null}{checkoutRepository && !historyDepthValid ? <p className="text-sm text-err" role="alert">History depth must be a positive whole number.</p> : null}{checkoutRepository && repositoryState.status === "ready" && !defaultWorkspaceRepository?.ready ? <p className="text-sm text-warn">No approved repository is available for this workspace.</p> : null}</div> : null}
    {!exactTask && sourceKind === "repository" ? <div className="grid gap-4"><label className="grid gap-2 text-sm font-semibold">Repository<input className={`${fieldClass} font-mono`} onChange={(event) => setRepository(event.target.value)} value={repository} /></label><div className="grid gap-4 md:grid-cols-2"><label className="grid gap-2 text-sm font-semibold">Ref or immutable commit<input className={`${fieldClass} font-mono`} onChange={(event) => setRevision(event.target.value)} value={revision} /></label><label className="grid gap-2 text-sm font-semibold">Package path<input className={`${fieldClass} font-mono`} onChange={(event) => setPath(event.target.value)} value={path} /></label></div><p className="text-xs text-muted-ink">The repository must be in the operator&apos;s allowed source list. Steward resolves a ref to an exact commit and validates the package&apos;s declared requirements before execution.</p></div> : null}
    {!exactTask && sourceKind === "registry" ? <label className="grid gap-2 text-sm font-semibold">Published workflow<select className={fieldClass} onChange={(event) => setWorkflowRef(event.target.value)} value={workflowRef}>{data.workflows.workflows.map((workflow) => <option key={`${workflow.name}@${workflow.version}`} value={`${workflow.name}@${workflow.version}`}>{workflow.displayName} · {workflow.name}@{workflow.version}</option>)}</select></label> : null}
    <label className="grid gap-2 text-sm font-semibold">Inputs (JSON object)<span className="text-xs font-normal text-muted-ink">Optional JSON (up to 16 KiB), available to the agent as <code>in/inputs.json</code>. Reference it in your prompt; it cannot change the agent, model, tools, or Envelope. Example: <code>{'{"release":"v1.2.3"}'}</code>.</span><textarea className="min-h-28 rounded-control border bg-panel p-3 font-mono text-xs font-normal" onChange={(event) => setInputs(event.target.value)} spellCheck={false} value={inputs} /></label>
    <label className="flex items-start gap-3 rounded-control border border-warn/30 bg-warn-soft p-4 text-sm"><input checked={captureExecutionLog} className="mt-1 size-4" onChange={(event) => setCaptureExecutionLog(event.target.checked)} type="checkbox" /><span><strong className="block font-semibold text-warn">Capture execution log</strong><span className="mt-1 block text-muted-ink">Retain stdout and stderr for this run. Execution logs may reproduce arbitrary user, tool, or agent output.</span></span></label>
    {status === "error" ? <div className="text-sm text-err" role="alert"><p>{failure ?? genericRunNowFailure}</p><DiagnosticDetails className="mt-1" items={failureDetails} /></div> : null}
    <div className="flex justify-end"><button className="rounded-control bg-brand px-5 py-2.5 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={status === "submitting" || (!exactTask && sourceKind === "inline" && (!inlineAllowed || !selectedModel || !workspaceCheckoutReady))} type="submit">{status === "submitting" ? "Starting…" : "Run now"}</button></div>
  </form>;
}
