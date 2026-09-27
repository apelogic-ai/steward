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
import { DataTable, FilterTabs, SectionCard } from "@/components/hs";
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
    { key: "workflow", label: "Workflow", className: "font-semibold", render: (run: BrowserRunView) => <span className="block truncate">{run.workflow}</span> },
    ...(admin ? [{ key: "owner", label: "Owner", render: (run: BrowserRunView) => <span className="block truncate text-muted-ink">{"ownerDisplayEmail" in run ? String(run.ownerDisplayEmail ?? "Not reported") : "Not reported"}</span> }] : []),
    { key: "status", label: "Status", render: (run: BrowserRunView) => <StatusBadge value={run.phase} /> },
    { key: "runtime", label: "Runtime", className: "font-mono text-xs text-muted-ink", render: (run: BrowserRunView) => runtimeLabel(run.runtimeUid) },
    { key: "spend", label: "Spend", className: "tabular-nums", render: (run: BrowserRunView) => run.observedSpend ? `${run.observedSpend.observedAmount} ${run.observedSpend.currency}` : "—" },
    { key: "updated", label: "Updated", className: "text-muted-ink", render: (run: BrowserRunView) => dateTime(run.updatedAt) },
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
        description={admin ? "Inspect the administrator-authorized run view." : "Track governed execution using the authoritative run record."}
        title={admin ? "All runs" : "Runs"}
      />
      <ResourceBoundary state={state}>{(data) => (
        <div className="space-y-5">
          <FilterTabs
            active={phase}
            items={[
              { count: data.runs.length, label: "All", value: "all" },
              ...Object.entries(data.facets.phase).filter(([, count]) => count > 0).map(([value, count]) => ({ count, label: value.charAt(0).toUpperCase() + value.slice(1), value })),
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
          <article className="space-y-6">
            <header className="rounded-panel border bg-panel p-5 shadow-sm sm:p-6">
              <div className="flex flex-wrap items-start justify-between gap-4">
                <div><p className="text-xs font-semibold uppercase tracking-wide text-muted-ink">Workflow run</p><h2 className="mt-1 text-2xl font-semibold tracking-tight">{run.workflow}</h2><p className="mt-1 break-all font-mono text-xs text-muted-ink">{run.taskUid}</p></div>
                <StatusBadge value={run.phase} />
              </div>
              <div className="mt-5 flex flex-wrap items-center gap-x-5 gap-y-2 border-t border-line-soft pt-4 text-sm text-muted-ink"><span>Started {dateTime(run.createdAt)}</span><span>Duration {durationLabel(run.createdAt, run.updatedAt)}</span></div>
              {!admin ? <div className="mt-5 flex flex-wrap gap-3"><button className="min-h-10 rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={rerunState === "working"} onClick={async () => {
                if (session.status !== "authenticated") return;
                setRerunState("working");
                const idempotencyKey = crypto.randomUUID();
                const outcome = await pollRerun(() => rerunMyRun({ body: { idempotencyKey }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { task_uid: taskUid } }));
                if ("taskUid" in outcome) router.push(`/runs/${outcome.taskUid}`);
                else setRerunState(outcome.failure);
              }} type="button">{rerunState === "working" ? "Starting…" : "Re-run"}</button>{!isTerminalPhase(run.phase) ? <button className="min-h-10 rounded-control border border-danger-line px-4 py-2 text-sm font-semibold text-err disabled:opacity-50" disabled={cancelState === "working" || cancelState === "cancelled"} onClick={async () => {
                if (session.status !== "authenticated") return;
                setCancelState("working");
                const result = await cancelMyRun({ cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { task_uid: taskUid } });
                setCancelState(result.data && result.response?.ok ? "cancelled" : classifyMutationFailure(result.response?.status));
              }} type="button">{cancelState === "working" ? "Cancelling…" : cancelState === "cancelled" ? "Cancellation requested" : "Cancel run"}</button> : null}</div> : null}
              {rerunState !== "idle" && rerunState !== "working" ? <p className="mt-3 text-sm text-err" role="alert">The run could not be re-run ({rerunState}).</p> : null}
              {cancelState !== "idle" && cancelState !== "working" && cancelState !== "cancelled" ? <p className="mt-3 text-sm text-err" role="alert">The run could not be cancelled ({cancelState}).</p> : null}
            </header>
            <div className="grid items-start gap-6 xl:grid-cols-[minmax(0,1.45fr)_minmax(300px,0.72fr)]">
              <section aria-labelledby="stages-title" className="overflow-hidden rounded-panel border bg-panel shadow-sm">
                <div className="border-b border-line px-5 py-4"><h3 className="font-semibold" id="stages-title">Jobs</h3><p className="mt-1 text-sm text-muted-ink">Governed execution stages and captured output.</p></div>
                {run.stages.length ? <ol className="divide-y divide-line-soft">{run.stages.map((stage, index) => <li key={stage.id}><details className="group" open><summary className="flex cursor-pointer list-none items-center gap-3 px-5 py-4"><span aria-hidden="true" className={`grid size-6 shrink-0 place-items-center rounded-full text-xs font-bold ${stage.state === "succeeded" ? "bg-ok-soft text-ok" : stage.state === "failed" ? "bg-err-soft text-err" : "bg-info-soft text-info"}`}>{stage.state === "succeeded" ? "✓" : stage.state === "failed" ? "×" : index + 1}</span><span className="min-w-0 flex-1 font-semibold">{stage.displayName}</span><StatusBadge value={stage.state} /><span aria-hidden="true" className="text-muted-ink transition-transform group-open:rotate-90">›</span></summary>{stage.steps.length ? <div className="border-t border-line-soft bg-subtle px-5 py-3 ps-14">{stage.steps.map((step) => <div className="flex flex-wrap items-center justify-between gap-3 py-2" key={step.id}><span className="text-sm">{step.displayName}</span><div className="flex gap-2">{step.logStreams.map((stream) => <a className="rounded-control border bg-panel px-3 py-1.5 font-mono text-xs font-semibold" href={`${admin ? "/admin/runs" : "/runs"}/${encodeURIComponent(taskUid)}/logs/${stream}`} key={stream}>{stream}</a>)}</div></div>)}</div> : null}</details></li>)}</ol> : <div className="p-5"><EmptyState title="No stages reported" /></div>}
              </section>
              <aside className="space-y-5">
                <SectionCard title="Summary"><DefinitionList items={detailItems} /></SectionCard>
                {run.trigger ? <SectionCard title="Triggered by GitHub"><DefinitionList items={[["Repository", run.trigger.repository], ["Event", run.trigger.event], ["Actor", run.trigger.actor], ["Ref", run.trigger.ref], ["Commit", run.trigger.sha], ["Workflow", run.trigger.callerWorkflow]]} /><a className="mt-4 inline-flex text-sm font-semibold text-link hover:text-link-hover" href={run.trigger.runUrl} rel="noreferrer" target="_blank">Open GitHub run ↗</a></SectionCard> : null}
              </aside>
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
