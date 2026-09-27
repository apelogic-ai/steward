"use client";

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
import { DataTable, FilterTabs } from "@/components/hs";
import { classifyMutationFailure, type MutationFailureState } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";
import { EmptyState, PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";

function dateTime(value: string): string {
  const parsed = new Date(value);
  return Number.isNaN(parsed.valueOf()) ? value : parsed.toLocaleString();
}

function runUpdatedAt(value: BrowserRunView): number {
  const parsed = new Date(value.updatedAt).valueOf();
  return Number.isNaN(parsed) ? Number.NEGATIVE_INFINITY : parsed;
}

function runtimeLabel(value: string | null | undefined): string {
  return value || "unassigned";
}

function relativeTime(value: string): string {
  const milliseconds = new Date(value).valueOf();
  if (!Number.isFinite(milliseconds)) return value;
  const seconds = Math.max(0, Math.round((Date.now() - milliseconds) / 1000));
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.round(hours / 24);
  return days === 1 ? "Yesterday" : `${days} days ago`;
}

function durationLabel(createdAt: string, updatedAt: string): string {
  const start = new Date(createdAt).valueOf();
  const end = new Date(updatedAt).valueOf();
  if (!Number.isFinite(start) || !Number.isFinite(end) || end < start) return "Not reported";
  const seconds = Math.round((end - start) / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const remainder = seconds % 60;
  return remainder ? `${minutes}m ${remainder}s` : `${minutes}m`;
}

function isTerminalPhase(phase: string): boolean {
  return phase === "failed" || phase === "succeeded" || phase === "cancelled";
}

type RerunAttempt = {
  data?: { retryAfterMs?: number; taskUid?: string };
  response?: { ok: boolean; status: number };
};

type RerunOutcome = { taskUid: string } | { failure: MutationFailureState };

function wait(milliseconds: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

export async function pollRerun(
  attempt: () => Promise<RerunAttempt>,
  pause: (milliseconds: number) => Promise<void> = wait,
  maxAttempts = 60,
): Promise<RerunOutcome> {
  for (let index = 0; index < maxAttempts; index += 1) {
    const result = await attempt();
    if (result.response?.ok && result.data?.taskUid) return { taskUid: result.data.taskUid };
    if (result.response?.status !== 202) {
      return { failure: classifyMutationFailure(result.response?.status) };
    }
    if (index + 1 < maxAttempts) {
      const retryAfterMs = Math.min(Math.max(result.data?.retryAfterMs ?? 1_000, 250), 5_000);
      await pause(retryAfterMs);
    }
  }
  return { failure: "unavailable" };
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
  const columns = [
    { key: "workflow", label: "Workflow", className: "font-semibold", render: (run: BrowserRunView) => <span><span className="block truncate">{run.workflow}</span><span className="mt-0.5 block truncate font-mono text-xs font-normal text-muted-ink">{run.taskUid}</span></span> },
    ...(admin ? [{ key: "owner", label: "Owner", render: (run: BrowserRunView) => <span className="block truncate text-muted-ink">{"ownerDisplayEmail" in run ? String(run.ownerDisplayEmail ?? "Not reported") : "Not reported"}</span> }] : []),
    { key: "status", label: "Status", render: (run: BrowserRunView) => <StatusBadge value={run.phase} /> },
    { key: "runtime", label: "Runtime", className: "font-mono text-xs text-muted-ink", render: (run: BrowserRunView) => <span className={run.runtimeUid ? "text-ink" : "text-faint-ink"}>{runtimeLabel(run.runtimeUid)}</span> },
    { key: "spend", label: "Spend", className: "text-right tabular-nums", render: (run: BrowserRunView) => run.observedSpend ? `${run.observedSpend.observedAmount} ${run.observedSpend.currency}` : "—" },
    { key: "updated", label: "Updated", className: "text-right text-muted-ink", render: (run: BrowserRunView) => relativeTime(run.updatedAt) },
  ];
  return <DataTable
    ariaLabel={admin ? "All runs" : "Runs"}
    columns={columns}
    gridTemplateColumns={admin ? "minmax(180px,1.5fr) minmax(180px,1fr) 120px 100px 100px 170px" : "minmax(220px,1.5fr) 120px 110px 100px 180px"}
    minWidth={admin ? "980px" : "780px"}
    rowHref={(run) => `${admin ? "/admin/runs" : "/runs"}/${run.taskUid}`}
    rowKey={(run) => run.taskUid}
    rows={newestRuns}
  />;
}

export function RunsView({ admin = false }: Readonly<{ admin?: boolean }>) {
  const [phase, setPhase] = useState("all");
  const load = useCallback(() => admin
    ? allRuns({ cache: "no-store", credentials: "same-origin" }) as Promise<{ data?: AllRunsResponse; response?: Response }>
    : myRuns({ cache: "no-store", credentials: "same-origin" }) as Promise<{ data?: MyRunsResponse; response?: Response }>, [admin]);
  const state = useApiResource<AllRunsResponse | MyRunsResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader
        description={admin ? "Every governed run across all users." : "Agent runs executed under your envelopes."}
        title={admin ? "All runs" : "Runs"}
      />
      <ResourceBoundary state={state}>{(data) => (
        <div className="space-y-5">
          <FilterTabs
            active={phase}
            items={[
              { count: data.runs.length, label: "All", value: "all" },
              ...(["running", "queued", "parked", "succeeded", "failed"] as const).map((value) => ({ count: data.facets.phase[value] ?? 0, label: value.charAt(0).toUpperCase() + value.slice(1), value })),
            ]}
            onChange={setPhase}
          />
          <RunCards admin={admin} runs={phase === "all" ? data.runs : data.runs.filter((run) => run.phase === phase)} />
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
  const [selectedStageId, setSelectedStageId] = useState<string | null>(null);
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
      <ResourceBoundary state={runState}>{({ run }) => {
        const pinnedWorkflow = run.workflowName && run.workflowVersion
          ? `${run.workflowName}@${run.workflowVersion}`
          : run.workflow;
        const selectedStage = run.stages.find((stage) => stage.id === selectedStageId) ?? run.stages.find((stage) => stage.state === "running" || stage.state === "failed") ?? run.stages[0];
        return (
          <article className="space-y-6">
            <header className="flex flex-wrap items-start justify-between gap-5">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-3"><h1 className="text-[28px] font-semibold tracking-tight" id="page-title">{pinnedWorkflow}</h1><StatusBadge value={run.phase} /></div>
                <p className="mt-1 break-all font-mono text-xs text-muted-ink">{run.taskUid}</p>
                {run.trigger ? <div className="mt-2 text-sm text-muted-ink">Triggered by <strong className="font-medium text-ink">{run.trigger.actor}</strong> via {run.trigger.event} · <a href={run.trigger.runUrl} rel="noreferrer" target="_blank">{run.trigger.repository}@{run.trigger.ref} ({run.trigger.sha.slice(0, 7)})</a> · {durationLabel(run.createdAt, run.updatedAt)}</div> : <div className="mt-2 text-sm text-muted-ink">Started {dateTime(run.createdAt)} · {durationLabel(run.createdAt, run.updatedAt)}</div>}
              </div>
              {!admin ? <div className="flex flex-wrap gap-2"><button className="min-h-10 rounded-control border bg-panel px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={rerunState === "working"} onClick={async () => {
                if (session.status !== "authenticated") return;
                setRerunState("working");
                const idempotencyKey = crypto.randomUUID();
                const outcome = await pollRerun(() => rerunMyRun({ body: { idempotencyKey }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { task_uid: taskUid } }));
                if ("taskUid" in outcome) router.push(`/runs/${outcome.taskUid}`);
                else setRerunState(outcome.failure);
              }} type="button">{rerunState === "working" ? "Starting…" : "Re-run"}</button>{!isTerminalPhase(run.phase) ? <button aria-label="Cancel run" className="min-h-10 rounded-control border border-danger-line px-4 py-2 text-sm font-semibold text-err disabled:opacity-50" disabled={cancelState === "working" || cancelState === "cancelled"} onClick={async () => {
                if (session.status !== "authenticated") return;
                setCancelState("working");
                const result = await cancelMyRun({ cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { task_uid: taskUid } });
                setCancelState(result.data && result.response?.ok ? "cancelled" : classifyMutationFailure(result.response?.status));
              }} type="button">{cancelState === "working" ? "Cancelling…" : cancelState === "cancelled" ? "Cancellation requested" : "···"}</button> : null}</div> : null}
            </header>
            {rerunState !== "idle" && rerunState !== "working" ? <p className="text-sm text-err" role="alert">The run could not be re-run ({rerunState}).</p> : null}
            {cancelState !== "idle" && cancelState !== "working" && cancelState !== "cancelled" ? <p className="text-sm text-err" role="alert">The run could not be cancelled ({cancelState}).</p> : null}
            <div className="grid min-h-[540px] overflow-hidden rounded-panel border bg-panel lg:grid-cols-[260px_minmax(0,1fr)]">
              <nav aria-label="Run jobs" className="border-b border-line lg:border-b-0 lg:border-r">
                <div className="border-b border-line-soft px-5 py-4 text-sm font-semibold">Jobs</div>
                {run.stages.length ? <ol className="p-2">{run.stages.map((stage) => <li key={stage.id}><button aria-current={stage.id === selectedStage?.id ? "true" : undefined} className={`flex w-full items-center gap-3 rounded-control px-3 py-3 text-left text-sm ${stage.id === selectedStage?.id ? "bg-brand-soft" : "hover:bg-subtle"}`} onClick={() => setSelectedStageId(stage.id)} type="button"><span aria-hidden="true" className={`size-2.5 shrink-0 rounded-full ${stage.state === "succeeded" ? "bg-ok" : stage.state === "failed" ? "bg-err" : stage.state === "running" ? "bg-info" : "bg-line"}`} /><span className="min-w-0 flex-1 font-medium">{stage.displayName}</span><span className="font-mono text-xs text-faint-ink">—</span></button></li>)}</ol> : <div className="p-5"><EmptyState title="No stages reported" /></div>}
                <div className="mx-5 border-t border-line-soft py-4 text-xs text-muted-ink"><p className="font-mono">{taskUid}</p><p className="mt-2">Created {dateTime(run.createdAt)}</p></div>
              </nav>
              <main className="min-w-0">
                <div className="grid border-b border-line-soft bg-subtle sm:grid-cols-2 xl:grid-cols-5">{[
                  ["Workflow version", pinnedWorkflow],
                  ["User envelope revision", run.userEnvelopeRevision ?? "—"],
                  ["Runtime", run.runtimeUid ?? "unassigned"],
                  ["Spend", run.observedSpend ? `${run.observedSpend.observedAmount} ${run.observedSpend.currency}` : "—"],
                  ["Agent", run.codingAgentRuntime],
                ].map(([label, value]) => <div className="min-w-0 border-b border-line-soft px-4 py-3 last:border-b-0 sm:border-r xl:border-b-0" key={label}><p className="text-xs text-muted-ink">{label}</p><p className="mt-1 truncate font-mono text-sm font-medium">{value}</p></div>)}</div>
                <section aria-labelledby="selected-stage" className="p-5">
                  <div className="flex items-center justify-between gap-4"><h2 className="text-lg font-semibold" id="selected-stage">{selectedStage?.displayName ?? "Run stages"}</h2>{selectedStage ? <StatusBadge value={selectedStage.state} /> : null}</div>
                  <p className="mt-5 rounded-control border border-warn/30 bg-warn-soft px-4 py-3 text-sm text-warn"><strong className="block font-semibold text-warn" id="sensitivity-notice">Sensitivity notice</strong>Execution logs may reproduce arbitrary user, tool, or agent output.</p>
                  {selectedStage?.steps.length ? <ol className="mt-5 divide-y divide-line-soft rounded-card border">{selectedStage.steps.map((step) => <li key={step.id}><details className="group"><summary className="flex cursor-pointer list-none items-center gap-3 px-4 py-3.5"><span aria-hidden="true" className={`grid size-5 place-items-center rounded-full text-xs ${step.state === "succeeded" ? "bg-ok-soft text-ok" : step.state === "failed" ? "bg-err-soft text-err" : "bg-info-soft text-info"}`}>{step.state === "succeeded" ? "✓" : step.state === "failed" ? "×" : "•"}</span><span className="min-w-0 flex-1 text-sm font-medium">{step.displayName}</span><span className="font-mono text-xs text-faint-ink">—</span><span aria-hidden="true" className="transition-transform group-open:rotate-90">›</span></summary>{step.logStreams.length ? <div className="border-t border-line-soft bg-subtle px-4 py-4"><div className="flex flex-wrap gap-2">{step.logStreams.map((stream) => <a className="rounded-control border bg-panel px-3 py-2 font-mono text-xs font-semibold" href={`${admin ? "/admin/runs" : "/runs"}/${encodeURIComponent(taskUid)}/logs/${stream}`} key={stream}>View {stream}</a>)}</div></div> : null}</details></li>)}</ol> : <div className="mt-5"><EmptyState title="No steps reported" /></div>}
                </section>
              </main>
            </div>
          </article>
        );
      }}</ResourceBoundary>
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
