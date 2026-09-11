"use client";

import Link from "next/link";
import { useCallback, useState } from "react";

import {
  allRun,
  allRuns,
  allRunTimeline,
  myRun,
  myRuns,
  myRunTimeline,
  type AllRunsResponse,
  type BrowserRunResponse,
  type BrowserRunTimelineResponse,
  type BrowserRunView,
  type MyRunsResponse,
} from "@/api-client";
import { useApiResource } from "@/data/use-api-resource";
import { DefinitionList, EmptyState, PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { authStartPath } from "@/session/auth-redirect";

type ExecutionLogStream = "stderr" | "stdout";

type ExecutionLogState =
  | { status: "idle" }
  | { status: "loading"; stream: ExecutionLogStream }
  | { status: "ready"; stream: ExecutionLogStream; text: string }
  | { status: "unavailable"; stream: ExecutionLogStream }
  | { status: "error"; stream: ExecutionLogStream };

function dateTime(value: string): string {
  const parsed = new Date(value);
  return Number.isNaN(parsed.valueOf()) ? value : parsed.toLocaleString();
}

function runUpdatedAt(value: BrowserRunView): number {
  const parsed = new Date(value.updatedAt).valueOf();
  return Number.isNaN(parsed) ? Number.NEGATIVE_INFINITY : parsed;
}

function runtimeLabel(value: string | null | undefined): string {
  if (!value) return "Not assigned";
  return value.split("-", 1)[0] || value;
}

function isTerminalPhase(phase: string): boolean {
  return phase === "failed" || phase === "succeeded";
}

function TerminalPhaseLogs({ admin, taskUid }: Readonly<{ admin: boolean; taskUid: string }>) {
  const [state, setState] = useState<ExecutionLogState>({ status: "idle" });

  async function load(stream: ExecutionLogStream) {
    setState({ status: "loading", stream });
    const prefix = admin ? "/admin/api/v1/all-runs" : "/app/api/v1/runs";
    try {
      const response = await fetch(`${prefix}/${encodeURIComponent(taskUid)}/logs/${stream}`, {
        cache: "no-store",
        credentials: "same-origin",
        headers: { accept: "text/plain" },
      });
      if (response.status === 401) {
        window.location.replace(authStartPath(window.location.pathname));
        return;
      }
      if (response.status === 404) {
        setState({ status: "unavailable", stream });
        return;
      }
      if (!response.ok) {
        setState({ status: "error", stream });
        return;
      }
      setState({ status: "ready", stream, text: await response.text() });
    } catch {
      setState({ status: "error", stream });
    }
  }

  const selectedStream = state.status === "idle" ? null : state.stream;
  return (
    <div className="mt-3 space-y-3">
      <div className="flex flex-wrap gap-2">
        {(["stdout", "stderr"] as const).map((stream) => (
          <button
            aria-pressed={selectedStream === stream}
            className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas disabled:cursor-wait disabled:opacity-50"
            disabled={state.status === "loading"}
            key={stream}
            onClick={() => void load(stream)}
            type="button"
          >
            {state.status === "loading" && state.stream === stream ? `Loading ${stream}…` : `View ${stream}`}
          </button>
        ))}
      </div>
      {state.status !== "idle" ? (
        <section aria-label="Execution log" className="space-y-3 rounded-md border bg-canvas p-4">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <h3 className="font-semibold">{state.stream} log</h3>
            <button className="min-h-11 rounded-md border px-3 py-2 text-sm font-semibold hover:bg-panel" onClick={() => setState({ status: "idle" })} type="button">Close log</button>
          </div>
          {state.status === "loading" ? <p className="text-sm text-muted-ink" role="status">Loading {state.stream} log…</p> : null}
          {state.status === "unavailable" ? <p className="text-sm text-muted-ink" role="status">{state.stream} log is unavailable for this run.</p> : null}
          {state.status === "error" ? <p className="text-sm text-red-800" role="alert">The {state.stream} log could not be loaded.</p> : null}
          {state.status === "ready" ? (
            <>
              <div className="rounded-md border border-amber-700/60 bg-amber-950/20 p-3 text-sm">
                <p className="font-semibold">Sensitive output warning</p>
                <p className="mt-1 text-muted-ink">Execution logs may reproduce arbitrary user, tool, or agent output.</p>
              </div>
              <pre className="max-h-[32rem] overflow-auto whitespace-pre-wrap break-words rounded-md border bg-panel p-4 font-mono text-xs">{state.text}</pre>
            </>
          ) : null}
        </section>
      ) : null}
    </div>
  );
}

export function RunCards({ admin = false, runs }: Readonly<{ admin?: boolean; runs: Array<BrowserRunView> }>) {
  if (runs.length === 0) return <EmptyState title="No data" />;
  const newestRuns = [...runs].sort((left, right) => runUpdatedAt(right) - runUpdatedAt(left));
  return (
    <ul className="grid gap-4">
      {newestRuns.map((run) => (
        <li className="rounded-panel border bg-panel px-5 py-4 shadow-sm" key={run.taskUid}>
          <div className="flex items-start justify-between gap-4">
            <div className="min-w-0">
              <p className="truncate font-semibold">{run.workflow}</p>
            </div>
            <StatusBadge value={run.phase} />
          </div>
          <div className="mt-4">
            <DefinitionList items={[
              ["Runtime", runtimeLabel(run.runtimeUid)],
              ["Updated", dateTime(run.updatedAt)],
              ["Spend", run.observedSpend ? `${run.observedSpend.observedAmount} ${run.observedSpend.currency}` : "Not reported"],
            ]} />
          </div>
          <Link className="inline-flex min-h-11 items-center text-sm font-semibold text-brand hover:text-brand-strong" href={`${admin ? "/admin/runs" : "/runs"}/${run.taskUid}`}>View run →</Link>
        </li>
      ))}
    </ul>
  );
}

export function RunsView({ admin = false }: Readonly<{ admin?: boolean }>) {
  const load = useCallback(() => admin
    ? allRuns({ cache: "no-store", credentials: "same-origin" }) as Promise<{ data?: AllRunsResponse; response?: Response }>
    : myRuns({ cache: "no-store", credentials: "same-origin" }) as Promise<{ data?: MyRunsResponse; response?: Response }>, [admin]);
  const state = useApiResource<AllRunsResponse | MyRunsResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader
        description={admin ? "Inspect the administrator-authorized run view." : "Track governed execution using the authoritative run record."}
        title={admin ? "All runs" : "Runs"}
      />
      <ResourceBoundary state={state}>{(data) => <RunCards admin={admin} runs={data.runs} />}</ResourceBoundary>
    </section>
  );
}

export function RunDetailView({ admin = false, taskUid }: Readonly<{ admin?: boolean; taskUid: string }>) {
  const loadRun = useCallback(() => admin
    ? allRun({ cache: "no-store", credentials: "same-origin", path: { task_uid: taskUid } })
    : myRun({ cache: "no-store", credentials: "same-origin", path: { task_uid: taskUid } }), [admin, taskUid]);
  const loadTimeline = useCallback(() => admin
    ? allRunTimeline({ cache: "no-store", credentials: "same-origin", path: { task_uid: taskUid } })
    : myRunTimeline({ cache: "no-store", credentials: "same-origin", path: { task_uid: taskUid } }), [admin, taskUid]);
  const runState = useApiResource<BrowserRunResponse>(loadRun);
  const timelineState = useApiResource<BrowserRunTimelineResponse>(loadTimeline);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Inspect status, bounded spend, and the append-only timeline." title="Run detail" />
      <ResourceBoundary state={runState}>{({ run }) => {
        const pinnedWorkflow = run.workflowName && run.workflowVersion
          ? `${run.workflowName}@${run.workflowVersion}`
          : run.workflow;
        return (
        <article className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div><h2 className="text-xl font-semibold">{run.workflow}</h2><p className="mt-1 break-all font-mono text-xs text-muted-ink">{run.taskUid}</p></div>
            <StatusBadge value={run.phase} />
          </div>
          <DefinitionList items={[
            ["Workflow version", pinnedWorkflow],
            ["Coding agent", run.codingAgentRuntime],
            ["Runtime UID", run.runtimeUid ?? "Not assigned"],
            ["Ownership", run.runtimeOwnership],
            ["User envelope revision", run.userEnvelopeRevision ?? "Not reported"],
            ["Service envelope revision", run.envelopeRevision ?? "Not reported"],
            ["Created", dateTime(run.createdAt)],
            ["Updated", dateTime(run.updatedAt)],
            ["Observed spend", run.observedSpend ? `${run.observedSpend.observedAmount} ${run.observedSpend.currency}` : "Not reported"],
            ["Error category", run.errorCategory ?? "None reported"],
            ["Finalized", run.finalized ? "Yes" : run.finalizationRequested ? "Requested" : "No"],
          ]} />
        </article>
      );}}</ResourceBoundary>
      <div className="space-y-3">
        <h2 className="text-xl font-semibold">Timeline</h2>
        <ResourceBoundary state={timelineState}>{({ events }) => events.length === 0 ? (
          <EmptyState title="No data" />
        ) : (
          <ol className="space-y-3 border-s-2 ps-5">
            {events.map((event, index) => (
              <li className="relative rounded-panel border bg-panel p-4" key={`${event.at}-${index}`}>
                <span aria-hidden="true" className="absolute -start-[1.63rem] top-5 size-3 rounded-full bg-brand" />
                <p className="font-semibold capitalize">{event.kind.replaceAll(/([A-Z])/g, " $1")}</p>
                {event.kind === "phase" ? (
                  <>
                    <StatusBadge value={event.phase} />
                    {isTerminalPhase(event.phase) ? <TerminalPhaseLogs admin={admin} taskUid={taskUid} /> : null}
                  </>
                ) : null}
                <time className="mt-2 block text-xs text-muted-ink">{dateTime(event.at)}</time>
              </li>
            ))}
          </ol>
        )}</ResourceBoundary>
      </div>
    </section>
  );
}
