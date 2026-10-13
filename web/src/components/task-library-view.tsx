"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useId, useMemo, useState, type FormEvent } from "react";

import {
  getUserCapabilities,
  getStarterTask,
  listRequests,
  myTask,
  myTasks,
  saveMyTask,
  type BrowserTaskResponse,
  type BrowserTasksResponse,
  type BrowserTaskView,
  type EnvelopeRequestsResponse,
  type StarterTaskResponse,
  type UserCapabilitiesResponse,
} from "@/api-client";
import {
  compatibleAgents,
  effectiveAgentSelection,
  effectiveModelSelection,
  ModelPicker,
  preferredTaskModel,
  requirementsWithSelectedModel,
  qualifiedEnvelopeDigest,
  startingSelection,
  type Model,
} from "@/components/model-selection";
import { WorkspaceRepositoryControls } from "@/components/workspace-repository-controls";
import { PageHeader, ResourceBoundary } from "@/components/workspace-ui";
import { RepositoryResource, useGithubRepositories } from "@/data/github-repositories";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

const fieldClass = "min-h-11 w-full rounded-control border bg-panel px-3 font-normal";

async function loadTask(contentDigest: string): Promise<{ data?: BrowserTaskResponse; response?: Response }> {
  return myTask({ cache: "no-store", credentials: "same-origin", path: { content_digest: contentDigest }, query: { limit: 1 } });
}

export function TaskLibraryView() {
  const load = useCallback(() => myTasks({ cache: "no-store", credentials: "same-origin" }), []);
  const state = useApiResource<BrowserTasksResponse>(load);
  return <section aria-labelledby="page-title" className="space-y-6">
    <PageHeader
      actions={<Link className="inline-flex min-h-10 items-center rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand" href="/tasks/new">New Task</Link>}
      description="Review saved Task versions before running them. Browser drafts, exact Git packages, and published Workflows all appear here."
      title="Tasks"
    />
    <ResourceBoundary state={state}>{({ tasks }) => tasks.length === 0
      ? <div className="rounded-card border bg-panel p-6"><p className="text-sm text-muted-ink">No Tasks are available yet.</p><Link className="mt-4 inline-flex rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand" href="/tasks/new">Create your first Task</Link></div>
      : <div className="grid min-w-0 gap-3">{tasks.map((task) => <article className="min-w-0 rounded-card border bg-panel p-5" key={`${task.contentDigest}-${task.source}`}>
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div className="min-w-0">
            <div className="flex flex-wrap items-center gap-2"><h2 className="font-semibold">{task.name}@{task.version}</h2><span className="rounded-full border px-2 py-0.5 text-xs text-muted-ink">{task.source === "inline" ? "Draft" : task.source.startsWith("steward:registry/") ? "Published" : "Git"}</span>{task.owned ? <span className="rounded-full border px-2 py-0.5 text-xs text-muted-ink">Mine</span> : null}</div>
            <p className="mt-2 break-all font-mono text-xs text-muted-ink">{task.contentDigest}</p>
            <p className="mt-1 break-all text-xs text-muted-ink">{task.source} · {task.revision}</p>
          </div>
          <div className="flex flex-wrap gap-2"><Link className="inline-flex min-h-10 items-center rounded-control border px-4 text-sm font-semibold" href={`/tasks/${encodeURIComponent(task.contentDigest)}`}>Review</Link><Link className="inline-flex min-h-10 items-center rounded-control bg-brand px-4 text-sm font-semibold text-on-brand" href={`/runs/new?task=${encodeURIComponent(task.contentDigest)}`}>Run</Link></div>
        </div>
      </article>)}</div>}
    </ResourceBoundary>
  </section>;
}

type EditorData = {
  capabilities: UserCapabilitiesResponse;
  envelopes: EnvelopeRequestsResponse;
  starterTask: StarterTaskResponse["starterTask"];
  task: BrowserTaskView | null;
};

export function TaskEditorView({ contentDigest }: Readonly<{ contentDigest?: string }>) {
  const load = useCallback(async () => {
    const [capabilities, envelopes, starterTask, task] = await Promise.all([
      getUserCapabilities({ cache: "no-store", credentials: "same-origin" }),
      listRequests({ cache: "no-store", credentials: "same-origin" }),
      getStarterTask({ cache: "no-store", credentials: "same-origin" }),
      contentDigest ? loadTask(contentDigest) : Promise.resolve(null),
    ]);
    const response = !capabilities.response?.ok
      ? capabilities.response
      : !envelopes.response?.ok
        ? envelopes.response
        : !starterTask.response?.ok
          ? starterTask.response
        : task && !task.response?.ok
          ? task.response
          : capabilities.response;
    return {
      data: capabilities.data && envelopes.data && starterTask.data && (!contentDigest || task?.data)
        ? { capabilities: capabilities.data, envelopes: envelopes.data, starterTask: starterTask.data.starterTask, task: task?.data?.task ?? null }
        : undefined,
      response,
    };
  }, [contentDigest]);
  const state = useApiResource<EditorData>(load);
  return <section aria-labelledby="page-title"><ResourceBoundary state={state}>{(data) => <TaskEditor data={data} />}</ResourceBoundary></section>;
}

function parseDefinition(task: BrowserTaskView | null): Record<string, unknown> | null {
  if (!task) return null;
  const source = task.files[task.path];
  if (!source) return null;
  try {
    const parsed = JSON.parse(source);
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed as Record<string, unknown> : null;
  } catch {
    return null;
  }
}

function jsonText(value: unknown, fallback: unknown): string {
  return JSON.stringify(value ?? fallback, null, 2);
}

function TaskEditor({ data }: Readonly<{ data: EditorData }>) {
  const router = useRouter();
  const session = useSession();
  const { refresh: refreshRepositories, state: repositoryState } = useGithubRepositories();
  const existing = data.task;
  const definition = useMemo(() => parseDefinition(existing), [existing]);
  const existingRuntime = definition?.runtime && typeof definition.runtime === "object" ? definition.runtime as Record<string, unknown> : null;
  const nextVersion = existing ? Math.max(existing.version, ...existing.versions.map((version) => version.version)) + 1 : 1;
  const activeEnvelopes = data.envelopes.requests.filter((request) => request.status === "provisioned" && request.envelopeDigest);
  const configuredAgentRef = typeof existingRuntime?.agentRef === "string"
    ? existingRuntime.agentRef
    : data.starterTask.taskDefinition.runtime.agentRef;
  const configuredModel = existing
    ? existing.runtime.model ?? null
    : preferredTaskModel(data.starterTask.taskDefinition);
  const initialEnvelope = activeEnvelopes.find((request) => request.envelopeDigest
    && qualifiedEnvelopeDigest(request.envelopeDigest) === existing?.preferredEnvelopeDigest) ?? activeEnvelopes[0];
  const initialEnvelopeSpec = initialEnvelope?.approvedEnvelope ?? initialEnvelope?.requestedEnvelope;
  const initialSelection = startingSelection(
    data.capabilities.agents,
    initialEnvelopeSpec?.spec.llms ?? [],
    configuredAgentRef,
    configuredModel,
  );
  const initialAgentRef = existing ? configuredAgentRef : initialSelection.agent?.agentRef ?? configuredAgentRef;
  const [name, setName] = useState(typeof definition?.name === "string" ? definition.name : "my-task");
  const [path, setPath] = useState(existing?.path ?? "task-definition.json");
  const [envelopeId, setEnvelopeId] = useState(initialEnvelope?.id ?? "");
  const [agentRef, setAgentRef] = useState(initialAgentRef);
  const [agentChanged, setAgentChanged] = useState(false);
  const [model, setModel] = useState<Model | null>(existing ? existing.runtime.model ?? null : initialSelection.model);
  const [modelChanged, setModelChanged] = useState(false);
  const [prompt, setPrompt] = useState(typeof definition?.promptText === "string" ? definition.promptText : "Write the requested result under $STEWARD_OUTPUT_DIR/out/.");
  const [skills, setSkills] = useState(jsonText(definition?.skills, []));
  const [workspace, setWorkspace] = useState(jsonText(definition?.workspace, []));
  const [outputs, setOutputs] = useState(jsonText(definition?.outputs, [{ path: "out", kind: "directory", required: true }]));
  const [requires, setRequires] = useState(definition?.requires ? jsonText(definition.requires, {}) : "");
  const [additionalFiles, setAdditionalFiles] = useState(jsonText(existing ? Object.fromEntries(Object.entries(existing.files).filter(([filePath]) => filePath !== existing.path)) : {}, {}));
  const [sharedRoles, setSharedRoles] = useState(existing?.sharedRoles ?? []);
  const [status, setStatus] = useState<"idle" | "saving" | "error">("idle");
  const [failure, setFailure] = useState<string | null>(null);
  const agentWarningId = useId();
  const modelWarningId = useId();
  const selectedEnvelope = activeEnvelopes.find((request) => request.id === envelopeId) ?? activeEnvelopes[0];
  const envelope = selectedEnvelope?.approvedEnvelope ?? selectedEnvelope?.requestedEnvelope;
  const agentOptions = selectedEnvelope
    ? compatibleAgents(data.capabilities.agents, envelope?.spec.llms ?? [])
    : data.capabilities.agents.map((agent) => ({
        agentRef: agent.agentRef,
        displayName: agent.displayName ?? null,
        models: [],
        compatible: true,
        reason: null,
      }));
  const currentAgent = agentOptions.find((agent) => agent.agentRef === agentRef);
  const preserveCurrentAgent = Boolean(existing && !agentChanged && agentRef && (!currentAgent || !currentAgent.compatible));
  const effectiveAgent = effectiveAgentSelection(agentOptions, agentRef);
  const selectedAgent = preserveCurrentAgent ? undefined : effectiveAgent.selected;
  const effectiveAgentRef = preserveCurrentAgent || (!existing && agentRef && !selectedAgent) ? agentRef : selectedAgent?.agentRef ?? "";
  const preserveCurrentModel = Boolean(existing && !modelChanged);
  const effectiveModel = effectiveModelSelection(model, selectedAgent);
  const selectedModel = preserveCurrentModel ? model : effectiveModel.selected;
  const agentWarning = preserveCurrentAgent
    ? selectedEnvelope
      ? `${currentAgent?.displayName ?? agentRef} is not available with the selected Envelope. You can keep it for this draft or choose another.`
      : "No active Envelope is selected. You can keep the current coding agent for this draft."
    : effectiveAgent.warning;
  const modelWarning = preserveCurrentModel && model && !selectedAgent?.models.some((candidate) => candidate.provider === model.provider && candidate.model === model.model)
    ? selectedEnvelope
      ? `${model.provider}/${model.model} is not available with the selected Envelope and coding agent. You can keep it for this draft or choose another.`
      : "No active Envelope is selected. You can keep the current model for this draft."
    : effectiveModel.warning;

  function selectEnvelope(nextEnvelopeId: string) {
    setEnvelopeId(nextEnvelopeId);
    const nextEnvelope = activeEnvelopes.find((request) => request.id === nextEnvelopeId);
    const nextSpec = nextEnvelope?.approvedEnvelope ?? nextEnvelope?.requestedEnvelope;
    const nextSelection = startingSelection(
      data.capabilities.agents,
      nextSpec?.spec.llms ?? [],
      configuredAgentRef,
      configuredModel,
    );
    setAgentRef(nextSelection.agent?.agentRef ?? configuredAgentRef);
    setAgentChanged(Boolean(existing));
    setModel(nextSelection.model);
    setModelChanged(Boolean(existing));
  }

  if (existing && (!existing.editable || existing.source !== "inline" || !existing.taskId)) {
    return <div className="space-y-4"><PageHeader description="Only your own browser draft can create another version." title="Task is read-only" /><Link className="inline-flex rounded-control border px-4 py-2 text-sm font-semibold" href={`/tasks/${encodeURIComponent(existing.contentDigest)}`}>Back to Task</Link></div>;
  }

  function previewPackage(): { definition: Record<string, unknown>; files: Record<string, string> } | null {
    try {
      if (!effectiveAgentRef) return null;
      const parsedSkills = JSON.parse(skills);
      const parsedWorkspace = JSON.parse(workspace);
      const parsedOutputs = JSON.parse(outputs);
      const parsedRequires = requires.trim() ? JSON.parse(requires) : undefined;
      const parsedFiles = JSON.parse(additionalFiles);
      if (!Array.isArray(parsedSkills) || !Array.isArray(parsedWorkspace) || !Array.isArray(parsedOutputs) || !parsedFiles || typeof parsedFiles !== "object" || Array.isArray(parsedFiles)) return null;
      const selectedRequirements = parsedRequires === undefined
        ? undefined
        : selectedModel && (!existing || modelChanged)
          ? requirementsWithSelectedModel(parsedRequires, selectedModel)
          : parsedRequires;
      if (parsedRequires !== undefined && !selectedRequirements) return null;
      const taskDefinition: Record<string, unknown> = {
        schemaVersion: "steward.task-definition/v2",
        name,
        version: nextVersion,
        runtime: { agentRef: effectiveAgentRef, ...(selectedModel ? { model: selectedModel } : {}) },
        promptText: prompt,
        skills: parsedSkills,
        workspace: parsedWorkspace,
        outputs: parsedOutputs,
        ...(selectedRequirements === undefined ? {} : { requires: selectedRequirements }),
      };
      const files = Object.fromEntries(Object.entries(parsedFiles).filter((entry): entry is [string, string] => typeof entry[1] === "string"));
      if (Object.keys(files).length !== Object.keys(parsedFiles).length) return null;
      files[path] = JSON.stringify(taskDefinition, null, 2);
      return { definition: taskDefinition, files };
    } catch {
      return null;
    }
  }

  const preview = previewPackage();

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated" || !preview) {
      setFailure("Fix the invalid JSON fields before saving.");
      setStatus("error");
      return;
    }
    setFailure(null);
    setStatus("saving");
    const result = await saveMyTask({
      body: {
        taskId: existing?.taskId ?? null,
        path,
        files: preview.files,
        preferredEnvelopeDigest: selectedEnvelope?.envelopeDigest
          ? qualifiedEnvelopeDigest(selectedEnvelope.envelopeDigest)
          : null,
        sharedRoles,
      },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
    });
    if (result.data && result.response?.ok) {
      router.push(`/tasks/${encodeURIComponent(result.data.task.contentDigest)}`);
      return;
    }
    setFailure("Steward rejected this package. Check its paths, Task definition, skills, workspace, outputs, and requirements.");
    setStatus("error");
  }

  return <form className="space-y-6" onSubmit={(event) => void submit(event)}>
    <PageHeader description={`Save version ${nextVersion}. Review the exact package below before running it.`} title={existing ? `Edit ${existing.name}` : "New Task"} />
    <section className="grid gap-4 rounded-card border bg-panel p-6 md:grid-cols-2">
      <label className="grid gap-2 text-sm font-semibold">Name<input className={fieldClass} disabled={Boolean(existing)} onChange={(event) => setName(event.target.value)} required value={name} /></label>
      <label className="grid gap-2 text-sm font-semibold">Package path<input className={`${fieldClass} font-mono`} onChange={(event) => setPath(event.target.value)} required value={path} /></label>
      <label className="grid gap-2 text-sm font-semibold">Envelope<select className={fieldClass} disabled={activeEnvelopes.length === 0} onChange={(event) => selectEnvelope(event.target.value)} value={selectedEnvelope?.id ?? ""}>{activeEnvelopes.length === 0 ? <option value="">No active Envelope</option> : null}{activeEnvelopes.map((request) => <option key={request.id} value={request.id}>{request.templateId ?? "Custom"} · rev {request.approvedEnvelope?.revision ?? request.requestedEnvelope.revision}</option>)}</select></label>
      <label className="grid gap-2 text-sm font-semibold">Coding agent<select aria-describedby={agentWarning ? agentWarningId : undefined} className={`${fieldClass} font-mono`} disabled={agentOptions.length === 0 && !preserveCurrentAgent} onChange={(event) => { setAgentRef(event.target.value); setAgentChanged(true); setModel(null); setModelChanged(true); }} required value={effectiveAgentRef}>{agentOptions.length === 0 && !preserveCurrentAgent ? <option value="">No coding agent configured</option> : null}{preserveCurrentAgent ? <option value={agentRef}>Keep current: {agentRef}</option> : null}{agentOptions.filter((agent) => !preserveCurrentAgent || agent.agentRef !== agentRef).map((agent) => <option disabled={!agent.compatible} key={agent.agentRef} value={agent.agentRef}>{agent.displayName ? `${agent.displayName} · ${agent.agentRef}` : agent.agentRef}{agent.compatible ? "" : " · unavailable"}</option>)}</select></label>
      <ModelPicker allowNoModel={preserveCurrentModel && model === null} className={fieldClass} describedBy={modelWarning ? modelWarningId : undefined} models={selectedAgent?.models ?? []} onChange={(nextModel) => { setModel(nextModel); setModelChanged(true); }} preserveUnavailable={preserveCurrentModel} selected={selectedModel} />
      {agentWarning ? <p className="text-sm text-warn" id={agentWarningId} role="status">{agentWarning}</p> : null}
      {modelWarning ? <p className="text-sm text-warn" id={modelWarningId} role="status">{modelWarning}</p> : null}
      <label className="grid gap-2 text-sm font-semibold md:col-span-2">Prompt<textarea className="min-h-40 rounded-control border bg-panel p-3 font-normal" onChange={(event) => setPrompt(event.target.value)} required value={prompt} /></label>
      <label className="grid gap-2 text-sm font-semibold">Skills (JSON array)<textarea className="min-h-32 rounded-control border bg-panel p-3 font-mono text-xs font-normal" onChange={(event) => setSkills(event.target.value)} spellCheck={false} value={skills} /></label>
      <div className="space-y-3 rounded-control border p-4 md:col-span-2"><div><h2 className="text-sm font-semibold">Repository checkout</h2><p className="mt-1 text-xs text-muted-ink">Choose an approved repository and a branch or tag. Steward pins the selected revision before the run starts.</p></div><RepositoryResource onRetry={refreshRepositories} state={repositoryState}>{({ repositories }) => <WorkspaceRepositoryControls onChange={setWorkspace} repositories={repositories} value={workspace} />}</RepositoryResource></div>
      <label className="grid gap-2 text-sm font-semibold">Workspace (advanced JSON array)<textarea className="min-h-32 rounded-control border bg-panel p-3 font-mono text-xs font-normal" onChange={(event) => setWorkspace(event.target.value)} placeholder={'[{"git": {}}, {"scratch": {"size": "2Gi"}}]'} spellCheck={false} value={workspace} /></label>
      <label className="grid gap-2 text-sm font-semibold">Outputs (JSON array)<textarea className="min-h-32 rounded-control border bg-panel p-3 font-mono text-xs font-normal" onChange={(event) => setOutputs(event.target.value)} spellCheck={false} value={outputs} /></label>
      <label className="grid gap-2 text-sm font-semibold">Requirements (optional JSON)<textarea className="min-h-36 rounded-control border bg-panel p-3 font-mono text-xs font-normal" onChange={(event) => setRequires(event.target.value)} placeholder="Omitted: use the selected Envelope maximum" spellCheck={false} value={requires} /></label>
      <label className="grid gap-2 text-sm font-semibold">Additional package files (JSON object)<textarea className="min-h-36 rounded-control border bg-panel p-3 font-mono text-xs font-normal" onChange={(event) => setAdditionalFiles(event.target.value)} spellCheck={false} value={additionalFiles} /></label>
      {session.status === "authenticated" && session.value.memberRoles.length > 0 ? <fieldset className="md:col-span-2"><legend className="text-sm font-semibold">Share with roles</legend><div className="mt-2 flex flex-wrap gap-4">{session.value.memberRoles.map((role) => <label className="flex items-center gap-2 text-sm" key={role}><input checked={sharedRoles.includes(role)} onChange={(event) => setSharedRoles((current) => event.target.checked ? [...current, role] : current.filter((value) => value !== role))} type="checkbox" />{role}</label>)}</div></fieldset> : null}
    </section>
    <section className="rounded-card border bg-panel p-5"><h2 className="font-semibold">Review exact Task definition</h2>{preview ? <pre className="mt-3 max-h-[36rem] overflow-auto rounded-control border bg-subtle p-4 text-xs">{JSON.stringify(preview.definition, null, 2)}</pre> : <p className="mt-3 text-sm text-err" role="alert">{selectedEnvelope && (!selectedAgent || !selectedModel) && !preserveCurrentAgent && !preserveCurrentModel ? "Choose a compatible coding agent and model." : "One or more JSON fields are invalid."}</p>}</section>
    {status === "error" ? <p className="text-sm text-err" role="alert">{failure}</p> : null}
    <div className="flex justify-end"><button className="rounded-control bg-brand px-5 py-2.5 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={!preview || status === "saving" || !effectiveAgentRef} type="submit">{status === "saving" ? "Saving…" : `Save version ${nextVersion}`}</button></div>
  </form>;
}
