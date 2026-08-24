"use client";

import Link from "next/link";
import { useCallback } from "react";

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

function dateTime(value: string): string {
  const parsed = new Date(value);
  return Number.isNaN(parsed.valueOf()) ? value : parsed.toLocaleString();
}

export function RunCards({ admin = false, runs }: Readonly<{ admin?: boolean; runs: Array<BrowserRunView> }>) {
  if (runs.length === 0) return <EmptyState title="No runs"><p>No authoritative run records match this view.</p></EmptyState>;
  return (
    <ul className="grid gap-4 lg:grid-cols-2">
      {runs.map((run) => (
        <li className="rounded-panel border bg-panel p-5 shadow-sm" key={run.taskUid}>
          <div className="flex items-start justify-between gap-4">
            <div className="min-w-0">
              <p className="truncate font-semibold">{run.workflow}</p>
              <p className="mt-1 break-all font-mono text-xs text-muted-ink">{run.taskUid}</p>
            </div>
            <StatusBadge value={run.phase} />
          </div>
          <DefinitionList items={[
            ["Runtime", run.runtimeUid ?? "Not assigned"],
            ["Updated", dateTime(run.updatedAt)],
            ["Spend", run.observedSpend ? `${run.observedSpend.observedAmount} ${run.observedSpend.currency}` : "Not reported"],
          ]} />
          <Link className="mt-5 inline-flex min-h-11 items-center text-sm font-semibold text-brand hover:text-brand-strong" href={`${admin ? "/admin/runs" : "/runs"}/${run.taskUid}`}>View run →</Link>
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
        eyebrow={admin ? "Administration" : "Governed execution"}
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
      <PageHeader description="Inspect status, bounded spend, and the append-only timeline." eyebrow={admin ? "Administration" : "Governed execution"} title="Run detail" />
      <ResourceBoundary state={runState}>{({ run }) => (
        <article className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div><h2 className="text-xl font-semibold">{run.workflow}</h2><p className="mt-1 break-all font-mono text-xs text-muted-ink">{run.taskUid}</p></div>
            <StatusBadge value={run.phase} />
          </div>
          <DefinitionList items={[
            ["Coding agent", run.codingAgentRuntime],
            ["Runtime UID", run.runtimeUid ?? "Not assigned"],
            ["Ownership", run.runtimeOwnership],
            ["Envelope revision", run.envelopeRevision ?? "Not reported"],
            ["Created", dateTime(run.createdAt)],
            ["Updated", dateTime(run.updatedAt)],
            ["Observed spend", run.observedSpend ? `${run.observedSpend.observedAmount} ${run.observedSpend.currency}` : "Not reported"],
            ["Error category", run.errorCategory ?? "None reported"],
            ["Finalized", run.finalized ? "Yes" : run.finalizationRequested ? "Requested" : "No"],
          ]} />
        </article>
      )}</ResourceBoundary>
      <div className="space-y-3">
        <h2 className="text-xl font-semibold">Timeline</h2>
        <ResourceBoundary state={timelineState}>{({ events }) => events.length === 0 ? (
          <EmptyState title="No timeline events"><p>The authoritative history is empty.</p></EmptyState>
        ) : (
          <ol className="space-y-3 border-s-2 ps-5">
            {events.map((event, index) => (
              <li className="relative rounded-panel border bg-panel p-4" key={`${event.at}-${index}`}>
                <span aria-hidden="true" className="absolute -start-[1.63rem] top-5 size-3 rounded-full bg-brand" />
                <p className="font-semibold capitalize">{event.kind.replaceAll(/([A-Z])/g, " $1")}</p>
                {event.kind === "phase" ? <StatusBadge value={event.phase} /> : null}
                <time className="mt-2 block text-xs text-muted-ink">{dateTime(event.at)}</time>
              </li>
            ))}
          </ol>
        )}</ResourceBoundary>
      </div>
    </section>
  );
}
