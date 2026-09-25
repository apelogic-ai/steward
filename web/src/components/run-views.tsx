"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useState } from "react";

import {
  allRun,
  allRuns,
  allRunTimeline,
  cancelMyRun,
  myRun,
  myRuns,
  myRunTimeline,
  rerunMyRun,
  type AllRunsResponse,
  type BrowserRunResponse,
  type BrowserRunTimelineResponse,
  type BrowserRunView,
  type MyRunsResponse,
} from "@/api-client";
import { classifyMutationFailure, type MutationFailureState } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";
import { DefinitionList, EmptyState, PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";

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
  return phase === "failed" || phase === "succeeded" || phase === "cancelled";
}

function TerminalPhaseLogs({ admin, taskUid }: Readonly<{ admin: boolean; taskUid: string }>) {
  const runPath = `${admin ? "/admin/runs" : "/runs"}/${encodeURIComponent(taskUid)}`;
  return (
    <div className="mt-3 flex flex-wrap gap-2">
      {(["stdout", "stderr"] as const).map((stream) => (
        <a
          className="inline-flex min-h-11 items-center rounded-md border px-4 py-2 text-sm font-semibold hover:bg-canvas"
          href={`${runPath}/logs/${stream}`}
          key={stream}
        >
          View {stream}
        </a>
      ))}
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
      <ResourceBoundary state={state}>{(data) => (
        <div className="space-y-5">
          <ul aria-label="Run phase counts" className="flex flex-wrap gap-2">
            {Object.entries(data.facets.phase).map(([phase, count]) => (
              <li className="rounded-full border bg-panel px-3 py-1 text-sm" key={phase}>{phase}: {count}</li>
            ))}
          </ul>
          <RunCards admin={admin} runs={data.runs} />
        </div>
      )}</ResourceBoundary>
    </section>
  );
}

export function RunDetailView({ admin = false, taskUid }: Readonly<{ admin?: boolean; taskUid: string }>) {
  const router = useRouter();
  const session = useSession();
  const [cancelState, setCancelState] = useState<"idle" | "working" | "cancelled" | MutationFailureState>("idle");
  const [rerunState, setRerunState] = useState<"idle" | "working" | MutationFailureState>("idle");
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
        const detailItems: Array<[string, string | number]> = [
          ["Workflow version", pinnedWorkflow],
          ["Coding agent", run.codingAgentRuntime],
          ["Runtime UID", run.runtimeUid ?? "Not assigned"],
          ["Ownership", run.runtimeOwnership],
          ["User envelope instance", run.userEnvelopeInstanceId ?? "Not reported"],
          ["User envelope revision", run.userEnvelopeRevision ?? "Not reported"],
          ["User envelope digest", run.userEnvelopeDigest ?? "Not reported"],
          ["Created", dateTime(run.createdAt)],
          ["Updated", dateTime(run.updatedAt)],
          ["Observed spend", run.observedSpend ? `${run.observedSpend.observedAmount} ${run.observedSpend.currency}` : "Not reported"],
          ["Error category", run.errorCategory ?? "None reported"],
          ["Finalized", run.finalized ? "Yes" : run.finalizationRequested ? "Requested" : "No"],
        ];
        if (admin && "ownerDisplayEmail" in run) detailItems.push(["Owner", String(run.ownerDisplayEmail ?? "Not reported")]);
        return (
        <article className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div><h2 className="text-xl font-semibold">{run.workflow}</h2><p className="mt-1 break-all font-mono text-xs text-muted-ink">{run.taskUid}</p></div>
            <StatusBadge value={run.phase} />
          </div>
          <DefinitionList items={detailItems} />
          {run.trigger ? (
            <section className="space-y-3 border-t pt-5">
              <h3 className="font-semibold">Triggered by GitHub</h3>
              <DefinitionList items={[
                ["Repository", run.trigger.repository],
                ["Event", run.trigger.event],
                ["Actor", run.trigger.actor],
                ["Ref", run.trigger.ref],
                ["Commit", run.trigger.sha],
                ["Workflow", run.trigger.callerWorkflow],
              ]} />
              <a className="text-sm font-semibold text-brand hover:text-brand-strong" href={run.trigger.runUrl} rel="noreferrer" target="_blank">Open GitHub run ↗</a>
            </section>
          ) : null}
          <section className="space-y-3 border-t pt-5">
            <h3 className="font-semibold">Stages</h3>
            <ol className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
              {run.stages.map((stage) => <li className="rounded-md border p-3" key={stage.id}><p className="text-sm font-semibold">{stage.displayName}</p><StatusBadge value={stage.state} />{stage.steps.map((step) => <div className="mt-3 border-t pt-3" key={step.id}><p className="text-sm">{step.displayName}</p><div className="mt-2 flex flex-wrap gap-2">{step.logStreams.map((stream) => <a className="text-xs font-semibold text-brand" href={`${admin ? "/admin/runs" : "/runs"}/${encodeURIComponent(taskUid)}/logs/${stream}`} key={stream}>{stream}</a>)}</div></div>)}</li>)}
            </ol>
          </section>
          {!admin ? <div className="flex flex-wrap gap-3 border-t pt-5"><button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50" disabled={rerunState === "working"} onClick={async () => {
            if (session.status !== "authenticated") return;
            setRerunState("working");
            const result = await rerunMyRun({ body: { idempotencyKey: crypto.randomUUID() }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { task_uid: taskUid } });
            if (result.data && result.response?.ok) router.push(`/runs/${result.data.taskUid}`);
            else setRerunState(classifyMutationFailure(result.response?.status));
          }} type="button">{rerunState === "working" ? "Starting…" : "Re-run"}</button>{rerunState !== "idle" && rerunState !== "working" ? <p className="self-center text-sm text-red-800" role="alert">The run could not be re-run ({rerunState}).</p> : null}</div> : null}
          {!admin && !isTerminalPhase(run.phase) ? (
            <div className="space-y-2 border-t pt-5">
              <button className="min-h-11 rounded-md border border-red-700 px-4 py-2 text-sm font-semibold text-red-800 disabled:opacity-50" disabled={cancelState === "working" || cancelState === "cancelled"} onClick={async () => {
                if (session.status !== "authenticated") return;
                setCancelState("working");
                const result = await cancelMyRun({ cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { task_uid: taskUid } });
                setCancelState(result.data && result.response?.ok ? "cancelled" : classifyMutationFailure(result.response?.status));
              }} type="button">{cancelState === "working" ? "Cancelling…" : cancelState === "cancelled" ? "Cancellation requested" : "Cancel run"}</button>
              {cancelState !== "idle" && cancelState !== "working" && cancelState !== "cancelled" ? <p className="text-sm text-red-800" role="alert">The run could not be cancelled ({cancelState}).</p> : null}
            </div>
          ) : null}
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
