"use client";

import Link from "next/link";
import { useCallback } from "react";

import { myTask, type BrowserTaskResponse, type BrowserTaskView } from "@/api-client";
import { RunCards, TaskGithubAutomationPanel } from "@/components/run-views";
import { DefinitionList, PageHeader, ResourceBoundary } from "@/components/workspace-ui";
import { useApiResource } from "@/data/use-api-resource";

async function loadTask(contentDigest: string): Promise<{ data?: BrowserTaskResponse; response?: Response }> {
  const first = await myTask({
    cache: "no-store",
    credentials: "same-origin",
    path: { content_digest: contentDigest },
    query: { limit: 100 },
  });
  if (!first.data || !first.response?.ok) return first;
  const task = first.data.task;
  const runs = [...task.runs];
  const seen = new Set<string>();
  let cursor = task.nextCursor;
  while (cursor) {
    if (seen.has(cursor)) return { data: undefined, response: new Response(null, { status: 502 }) };
    seen.add(cursor);
    const page = await myTask({
      cache: "no-store",
      credentials: "same-origin",
      path: { content_digest: contentDigest },
      query: { cursor, limit: 100 },
    });
    if (!page.data || !page.response?.ok) return page;
    runs.push(...page.data.task.runs);
    cursor = page.data.task.nextCursor;
  }
  return { data: { ...first.data, task: { ...task, runs, nextCursor: null } }, response: first.response };
}

function downloadTask(task: BrowserTaskView) {
  const blob = new Blob([JSON.stringify({ contentDigest: task.contentDigest, path: task.path, files: task.files }, null, 2)], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = `${task.name}-${task.version}.json`;
  anchor.click();
  URL.revokeObjectURL(url);
}

export function taskRunHref(task: Pick<BrowserTaskView, "contentDigest" | "name" | "source" | "version">): string {
  return task.source.startsWith("steward:registry/")
    ? `/runs/new?workflow=${encodeURIComponent(`${task.name}@${task.version}`)}`
    : `/runs/new?task=${encodeURIComponent(task.contentDigest)}`;
}

function TaskContent({ task }: Readonly<{ task: BrowserTaskView }>) {
  const model = task.runtime.model ? `${task.runtime.model.provider}/${task.runtime.model.model}` : "Selected by the Envelope";
  const runHref = taskRunHref(task);
  return <div className="space-y-6">
    <PageHeader
      actions={<div className="flex flex-wrap gap-2"><button className="min-h-10 rounded-control border bg-panel px-4 py-2 text-sm font-semibold" onClick={() => downloadTask(task)} type="button">Download</button><Link className="inline-flex min-h-10 items-center rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand" href={runHref}>Run</Link></div>}
      description="Immutable package definition and every run of this exact content digest."
      title={`${task.name}@${task.version}`}
    />
    <section className="rounded-card border bg-panel p-5">
      <DefinitionList items={[
        ["Content digest", <code className="break-all" key="digest">{task.contentDigest}</code>],
        ["Source", task.source],
        ["Revision", <code key="revision">{task.revision}</code>],
        ["Path", <code key="path">{task.path}</code>],
        ["Coding agent", <code key="agent">{task.runtime.agentRef}</code>],
        ["Model", <code key="model">{model}</code>],
        ["Authority", task.requires ? "Declared by this Task" : "Inherits the Envelope’s authority"],
      ]} />
      {task.requires ? <details className="mt-5"><summary className="cursor-pointer text-sm font-semibold">Required authority</summary><pre className="mt-3 overflow-auto rounded-control border bg-subtle p-4 text-xs">{JSON.stringify(task.requires, null, 2)}</pre></details> : null}
    </section>
    <section aria-labelledby="task-files-title" className="space-y-3">
      <h2 className="text-lg font-semibold" id="task-files-title">Files</h2>
      {Object.entries(task.files).map(([path, contents]) => <details className="rounded-card border bg-panel" key={path} open={path === task.path}><summary className="cursor-pointer px-4 py-3 font-mono text-sm font-semibold">{path}</summary><pre className="max-h-[32rem] overflow-auto whitespace-pre-wrap border-t bg-subtle p-4 text-xs">{contents}</pre></details>)}
    </section>
    <section aria-labelledby="task-runs-title" className="space-y-3">
      <h2 className="text-lg font-semibold" id="task-runs-title">Runs</h2>
      <RunCards runs={task.runs} />
    </section>
    {task.publicationTaskUid ? <TaskGithubAutomationPanel taskUid={task.publicationTaskUid} /> : <section className="rounded-card border bg-panel p-5"><h2 className="font-semibold">Use in GitHub Actions</h2><p className="mt-2 text-sm text-muted-ink">Complete a successful browser run of this Task before publishing its exact package to a repository.</p></section>}
  </div>;
}

export function TaskDetailView({ contentDigest }: Readonly<{ contentDigest: string }>) {
  const load = useCallback(() => loadTask(contentDigest), [contentDigest]);
  const state = useApiResource<BrowserTaskResponse>(load);
  return <section aria-labelledby="page-title"><ResourceBoundary state={state}>{({ task }) => <TaskContent task={task} />}</ResourceBoundary></section>;
}
