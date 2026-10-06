"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useMemo, useState, type FormEvent } from "react";

import {
  myTask,
  myTasks,
  saveMyTask,
  type BrowserTaskResponse,
  type BrowserTasksResponse,
  type BrowserTaskView,
} from "@/api-client";
import { PageHeader, ResourceBoundary } from "@/components/workspace-ui";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";
import { listPublishedWorkflows, type PublishedWorkflowListResponse } from "@/workflows/api";

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
      description="Review immutable package versions before running them. Saved drafts, exact Git packages, and published Workflows use the same Task surface."
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
  agents: PublishedWorkflowListResponse;
  task: BrowserTaskView | null;
};

export function TaskEditorView({ contentDigest }: Readonly<{ contentDigest?: string }>) {
  const load = useCallback(async () => {
    const [agents, task] = await Promise.all([
      listPublishedWorkflows(),
      contentDigest ? loadTask(contentDigest) : Promise.resolve(null),
    ]);
    const response = !agents.response?.ok ? agents.response : task && !task.response?.ok ? task.response : agents.response;
    return {
      data: agents.data && (!contentDigest || task?.data) ? { agents: agents.data, task: task?.data?.task ?? null } : undefined,
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
  const existing = data.task;
  const definition = useMemo(() => parseDefinition(existing), [existing]);
  const existingRuntime = definition?.runtime && typeof definition.runtime === "object" ? definition.runtime as Record<string, unknown> : null;
  const existingModel = existingRuntime?.model && typeof existingRuntime.model === "object" ? existingRuntime.model as Record<string, unknown> : null;
  const nextVersion = existing ? Math.max(existing.version, ...existing.versions.map((version) => version.version)) + 1 : 1;
  const agentRefs = [...new Set([...(data.agents.agents.map((agent) => agent.agentRef)), typeof existingRuntime?.agentRef === "string" ? existingRuntime.agentRef : ""].filter(Boolean))];
  const [name, setName] = useState(typeof definition?.name === "string" ? definition.name : "my-task");
  const [path, setPath] = useState(existing?.path ?? "task-definition.json");
  const [agentRef, setAgentRef] = useState(typeof existingRuntime?.agentRef === "string" ? existingRuntime.agentRef : agentRefs[0] ?? "");
  const [modelProvider, setModelProvider] = useState(typeof existingModel?.provider === "string" ? existingModel.provider : "");
  const [modelName, setModelName] = useState(typeof existingModel?.model === "string" ? existingModel.model : "");
  const [prompt, setPrompt] = useState(typeof definition?.promptText === "string" ? definition.promptText : "Write the requested result under $STEWARD_OUTPUT_DIR/out/.");
  const [skills, setSkills] = useState(jsonText(definition?.skills, []));
  const [outputs, setOutputs] = useState(jsonText(definition?.outputs, [{ path: "out", kind: "directory", required: true }]));
  const [requires, setRequires] = useState(definition?.requires ? jsonText(definition.requires, {}) : "");
  const [additionalFiles, setAdditionalFiles] = useState(jsonText(existing ? Object.fromEntries(Object.entries(existing.files).filter(([filePath]) => filePath !== existing.path)) : {}, {}));
  const [sharedRoles, setSharedRoles] = useState(existing?.sharedRoles ?? []);
  const [status, setStatus] = useState<"idle" | "saving" | "error">("idle");
  const [failure, setFailure] = useState<string | null>(null);

  if (existing && (!existing.editable || existing.source !== "inline" || !existing.taskId)) {
    return <div className="space-y-4"><PageHeader description="Only an owner-scoped draft can create another immutable version." title="Task is read-only" /><Link className="inline-flex rounded-control border px-4 py-2 text-sm font-semibold" href={`/tasks/${encodeURIComponent(existing.contentDigest)}`}>Back to Task</Link></div>;
  }

  function previewPackage(): { definition: Record<string, unknown>; files: Record<string, string> } | null {
    try {
      const parsedSkills = JSON.parse(skills);
      const parsedOutputs = JSON.parse(outputs);
      const parsedRequires = requires.trim() ? JSON.parse(requires) : undefined;
      const parsedFiles = JSON.parse(additionalFiles);
      if (!Array.isArray(parsedSkills) || !Array.isArray(parsedOutputs) || !parsedFiles || typeof parsedFiles !== "object" || Array.isArray(parsedFiles)) return null;
      const taskDefinition: Record<string, unknown> = {
        schemaVersion: "steward.task-definition/v2",
        name,
        version: nextVersion,
        runtime: { agentRef, ...(modelProvider.trim() && modelName.trim() ? { model: { provider: modelProvider.trim(), model: modelName.trim() } } : {}) },
        promptText: prompt,
        skills: parsedSkills,
        outputs: parsedOutputs,
        ...(parsedRequires === undefined ? {} : { requires: parsedRequires }),
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
      body: { taskId: existing?.taskId ?? null, path, files: preview.files, sharedRoles },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
    });
    if (result.data && result.response?.ok) {
      router.push(`/tasks/${encodeURIComponent(result.data.task.contentDigest)}`);
      return;
    }
    setFailure("Steward rejected this package. Check its paths, Task definition, skills, outputs, and requirements.");
    setStatus("error");
  }

  return <form className="space-y-6" onSubmit={(event) => void submit(event)}>
    <PageHeader description={`Save immutable version ${nextVersion}. Review the exact package below before running it.`} title={existing ? `Edit ${existing.name}` : "New Task"} />
    <section className="grid gap-4 rounded-card border bg-panel p-6 md:grid-cols-2">
      <label className="grid gap-2 text-sm font-semibold">Name<input className={fieldClass} disabled={Boolean(existing)} onChange={(event) => setName(event.target.value)} required value={name} /></label>
      <label className="grid gap-2 text-sm font-semibold">Package path<input className={`${fieldClass} font-mono`} onChange={(event) => setPath(event.target.value)} required value={path} /></label>
      <label className="grid gap-2 text-sm font-semibold">Coding agent<select className={`${fieldClass} font-mono`} onChange={(event) => setAgentRef(event.target.value)} required value={agentRef}>{agentRefs.map((ref) => <option key={ref} value={ref}>{ref}</option>)}</select></label>
      <div className="grid gap-4 sm:grid-cols-2"><label className="grid gap-2 text-sm font-semibold">Model provider<input className={fieldClass} onChange={(event) => setModelProvider(event.target.value)} placeholder="Optional" value={modelProvider} /></label><label className="grid gap-2 text-sm font-semibold">Model<input className={fieldClass} onChange={(event) => setModelName(event.target.value)} placeholder="Optional" value={modelName} /></label></div>
      <label className="grid gap-2 text-sm font-semibold md:col-span-2">Prompt<textarea className="min-h-40 rounded-control border bg-panel p-3 font-normal" onChange={(event) => setPrompt(event.target.value)} required value={prompt} /></label>
      <label className="grid gap-2 text-sm font-semibold">Skills (JSON array)<textarea className="min-h-32 rounded-control border bg-panel p-3 font-mono text-xs font-normal" onChange={(event) => setSkills(event.target.value)} spellCheck={false} value={skills} /></label>
      <label className="grid gap-2 text-sm font-semibold">Outputs (JSON array)<textarea className="min-h-32 rounded-control border bg-panel p-3 font-mono text-xs font-normal" onChange={(event) => setOutputs(event.target.value)} spellCheck={false} value={outputs} /></label>
      <label className="grid gap-2 text-sm font-semibold">Requirements (optional JSON)<textarea className="min-h-36 rounded-control border bg-panel p-3 font-mono text-xs font-normal" onChange={(event) => setRequires(event.target.value)} placeholder="Omitted: use the selected Envelope maximum" spellCheck={false} value={requires} /></label>
      <label className="grid gap-2 text-sm font-semibold">Additional package files (JSON object)<textarea className="min-h-36 rounded-control border bg-panel p-3 font-mono text-xs font-normal" onChange={(event) => setAdditionalFiles(event.target.value)} spellCheck={false} value={additionalFiles} /></label>
      {session.status === "authenticated" && session.value.memberRoles.length > 0 ? <fieldset className="md:col-span-2"><legend className="text-sm font-semibold">Share with roles</legend><div className="mt-2 flex flex-wrap gap-4">{session.value.memberRoles.map((role) => <label className="flex items-center gap-2 text-sm" key={role}><input checked={sharedRoles.includes(role)} onChange={(event) => setSharedRoles((current) => event.target.checked ? [...current, role] : current.filter((value) => value !== role))} type="checkbox" />{role}</label>)}</div></fieldset> : null}
    </section>
    <section className="rounded-card border bg-panel p-5"><h2 className="font-semibold">Review exact Task definition</h2>{preview ? <pre className="mt-3 max-h-[36rem] overflow-auto rounded-control border bg-subtle p-4 text-xs">{JSON.stringify(preview.definition, null, 2)}</pre> : <p className="mt-3 text-sm text-err" role="alert">One or more JSON fields are invalid.</p>}</section>
    {status === "error" ? <p className="text-sm text-err" role="alert">{failure}</p> : null}
    <div className="flex justify-end"><button className="rounded-control bg-brand px-5 py-2.5 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={!preview || status === "saving" || !agentRef} type="submit">{status === "saving" ? "Saving…" : `Save version ${nextVersion}`}</button></div>
  </form>;
}
