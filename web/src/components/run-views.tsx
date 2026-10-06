"use client";

import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { useCallback, useEffect, useState } from "react";

import {
  allRun,
  allRunTimeline,
  cancelMyRun,
  myRun,
  myRunOutputs,
  myRunPackage,
  myRunTimeline,
  renderRepositoryBundleForEnvelope,
  rerunMyRun,
  type AllRunsResponse,
  type BrowserRunResponse,
  type BrowserRunPackageContentResponse,
  type BrowserRunTimelineResponse,
  type BrowserRunOutputsResponse,
  type BrowserRunView,
  type MyRunsResponse,
} from "@/api-client";
import { DataTable, FilterChips } from "@/components/hs";
import { ConfirmationDialog } from "@/components/hs/confirmation-dialog";
import { classifyConnectionMutationFailure, type ConnectionMutationState } from "@/components/connection-mutation-state";
import { ExecutionLogPanel } from "@/components/run-log-view";
import { classifyMutationFailure, type MutationFailureState } from "@/data/mutation-state";
import type { ExecutionLogStream } from "@/data/execution-log";
import { useApiResource } from "@/data/use-api-resource";
import { loadAllEnvelopeRequests, loadAllMyRuns, loadAllRuns } from "@/data/paginated-api";
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

function durationBetween(start: string | undefined, end: string | undefined): string {
  return start && end ? durationLabel(start, end) : "—";
}

function stageDuration(stageId: string, createdAt: string, events: BrowserRunTimelineResponse["events"]): string {
  const at = (kind: BrowserRunTimelineResponse["events"][number]["kind"]) => events.find((event) => event.kind === kind)?.at;
  if (stageId === "admission") return durationBetween(createdAt, at("admitted"));
  if (stageId === "provision_runtime") return durationBetween(at("admitted"), at("runtimeBound"));
  if (stageId === "agent_execution") return durationBetween(at("executionStarted"), at("executionEnded"));
  if (stageId === "finalize") return durationBetween(at("finalizationRequested"), at("finalized"));
  return "—";
}

function isTerminalPhase(phase: string): boolean {
  return phase === "failed" || phase === "succeeded" || phase === "cancelled";
}

type RunEventSnapshot = {
  eventId: number;
  run: BrowserRunView;
  timeline: BrowserRunTimelineResponse;
};

export function parseRunEventSnapshot(body: string): RunEventSnapshot | null {
  const data = body
    .split("\n")
    .find((line) => line.startsWith("data: "))
    ?.slice("data: ".length);
  if (!data) return null;
  return parseRunEventData(data);
}

export function parseRunEventData(data: string): RunEventSnapshot | null {
  try {
    const value = JSON.parse(data) as Partial<RunEventSnapshot>;
    return typeof value.eventId === "number" && value.run && value.timeline
      ? value as RunEventSnapshot
      : null;
  } catch {
    return null;
  }
}

function stageSummary(stage: BrowserRunView["stages"][number]): string {
  if (stage.id === "admission") return "The Task was admitted under the selected Envelope.";
  if (stage.id === "provision_runtime") return stage.state === "succeeded"
    ? "The governed runtime was provisioned."
    : "Steward is preparing the governed runtime.";
  if (stage.id === "finalize") return stage.state === "succeeded"
    ? "The Task and its evidence were finalized."
    : "Steward will finalize the Task after execution ends.";
  return "No execution steps were reported.";
}

type RerunAttempt = {
  data?: { retryAfterMs?: number; taskUid?: string };
  error?: unknown;
  response?: { ok: boolean; status: number };
};

type RerunOutcome = { taskUid: string } | { failure: ConnectionMutationState };

export function rerunFailureMessage(state: ConnectionMutationState): string {
  return state === "orchestration-not-active"
    ? "Re-run is disabled until task orchestration is active (stage 2)."
    : `The run could not be re-run (${state}).`;
}

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
      return { failure: classifyConnectionMutationFailure(result.response?.status, result.error) };
    }
    if (index + 1 < maxAttempts) {
      const retryAfterMs = Math.min(Math.max(result.data?.retryAfterMs ?? 1_000, 250), 5_000);
      await pause(retryAfterMs);
    }
  }
  return { failure: "unavailable" };
}

function RunStepRow({ admin, onStreamChange, selectedStream, step, taskUid }: Readonly<{
  admin: boolean;
  onStreamChange: (stream: ExecutionLogStream) => void;
  selectedStream: ExecutionLogStream | null;
  step: BrowserRunView["stages"][number]["steps"][number];
  taskUid: string;
}>) {
  const streams = step.logStreams.filter((value): value is ExecutionLogStream => value === "stdout" || value === "stderr");
  const [open, setOpen] = useState(Boolean(selectedStream));
  const [localStream, setLocalStream] = useState<ExecutionLogStream>(streams[0] ?? "stdout");
  const stream = selectedStream && streams.includes(selectedStream) ? selectedStream : localStream;
  return (
    <li>
      <details className="group" onToggle={(event) => setOpen(event.currentTarget.open)} open={open || Boolean(selectedStream)}>
        <summary className="flex cursor-pointer list-none items-center gap-3 px-4 py-3.5">
          <span aria-hidden="true" className={`grid size-5 place-items-center rounded-full text-xs ${step.state === "succeeded" ? "bg-ok-soft text-ok" : step.state === "failed" ? "bg-err-soft text-err" : "bg-info-soft text-info"}`}>{step.state === "succeeded" ? "✓" : step.state === "failed" ? "×" : "•"}</span>
          <span className="min-w-0 flex-1 text-sm font-medium">{step.displayName}</span>
          <span aria-hidden="true" className="transition-transform group-open:rotate-90">›</span>
        </summary>
        {open && streams.length ? (
          <div className="space-y-3 border-t border-line-soft bg-subtle px-4 py-4">
            <div className="flex flex-wrap gap-2" role="tablist" aria-label={`${step.displayName} logs`}>
              {streams.map((availableStream) => <button aria-selected={stream === availableStream} className={`rounded-control border px-3 py-2 font-mono text-xs font-semibold ${stream === availableStream ? "border-brand bg-brand-soft" : "bg-panel"}`} key={availableStream} onClick={() => { setLocalStream(availableStream); onStreamChange(availableStream); }} role="tab" type="button">{availableStream}</button>)}
              <a className="ms-auto rounded-control border bg-panel px-3 py-2 font-mono text-xs font-semibold" href={`${admin ? "/admin/runs" : "/runs"}/${encodeURIComponent(taskUid)}/logs/${stream}`}>Open full page</a>
            </div>
            <ExecutionLogPanel admin={admin} stream={stream} taskUid={taskUid} />
          </div>
        ) : null}
      </details>
    </li>
  );
}

function RunOutputs({ taskUid }: Readonly<{ taskUid: string }>) {
  const load = useCallback(() => myRunOutputs({ cache: "no-store", credentials: "same-origin", path: { task_uid: taskUid } }), [taskUid]);
  const state = useApiResource<BrowserRunOutputsResponse>(load);
  return <ResourceBoundary state={state}>{({ files }) => files.length ? <div className="mt-5 rounded-card border p-4"><h3 className="text-sm font-semibold">Outputs</h3><ul className="mt-3 space-y-2">{files.map((file) => <li className="flex items-center justify-between gap-4 text-sm" key={file.path}><a className="font-mono font-semibold text-brand" href={file.downloadUrl}>{file.path}</a><span className="text-muted-ink">{file.sizeBytes} bytes</span></li>)}</ul></div> : <p className="mt-5 text-sm text-muted-ink">This run produced no output files.</p>}</ResourceBoundary>;
}

export function packagePrompt(files: Record<string, string>): { label: string; text: string } | null {
  const definitionSource = files["task-definition.json"];
  if (!definitionSource) return null;
  try {
    const definition = JSON.parse(definitionSource) as Record<string, unknown>;
    if (typeof definition.promptText === "string") {
      return { label: "Inline prompt", text: definition.promptText };
    }
    if (typeof definition.prompt === "string" && typeof files[definition.prompt] === "string") {
      return { label: definition.prompt, text: files[definition.prompt] };
    }
  } catch {
    return null;
  }
  return null;
}

function InlinePackageViewer({ taskUid }: Readonly<{ taskUid: string }>) {
  const load = useCallback(() => myRunPackage({ cache: "no-store", credentials: "same-origin", path: { task_uid: taskUid } }), [taskUid]);
  const state = useApiResource<BrowserRunPackageContentResponse>(load);
  return <ResourceBoundary state={state}>{({ files }) => {
    const prompt = packagePrompt(files);
    return <section className="mt-5 rounded-card border p-4" aria-labelledby="package-prompt-title">
      <h3 className="text-sm font-semibold" id="package-prompt-title">Task prompt</h3>
      {prompt ? <><p className="mt-2 font-mono text-xs text-muted-ink">{prompt.label}</p><pre className="mt-3 whitespace-pre-wrap rounded-control border bg-subtle p-4 text-sm">{prompt.text}</pre></> : <p className="mt-2 text-sm text-muted-ink">The exact package did not contain a readable prompt.</p>}
    </section>;
  }}</ResourceBoundary>;
}

export function exactRepositoryBundle(
  exactPackage: Record<string, string>,
  wrapperFiles: Record<string, string>,
): Record<string, string> {
  return { ...wrapperFiles, ...exactPackage };
}

function SaveInlineRunToRepository({ run }: Readonly<{ run: BrowserRunView }>) {
  const session = useSession();
  const [repository, setRepository] = useState("https://github.com/example-org/agentic-ops.git");
  const [state, setState] = useState<"idle" | "working" | "copied" | "error">("idle");
  const [failure, setFailure] = useState<string | null>(null);

  async function copyBundle() {
    if (session.status !== "authenticated" || !run.package || !run.userEnvelopeInstanceId) return;
    setState("working");
    setFailure(null);
    try {
      const requests = await loadAllEnvelopeRequests("provisioned");
      const envelopeRequest = requests.data?.requests.find(
        (request) => request.envelopeInstanceId === run.userEnvelopeInstanceId,
      );
      if (!envelopeRequest) {
        setFailure("The Envelope used by this run is no longer provisioned, so Steward cannot render its GitHub Actions wrapper.");
        setState("error");
        return;
      }
      const [exact, wrapper] = await Promise.all([
        myRunPackage({ cache: "no-store", credentials: "same-origin", path: { task_uid: run.taskUid } }),
        renderRepositoryBundleForEnvelope({
          body: {
            repository: repository.trim(),
            packagePath: run.package.path,
          },
          credentials: "same-origin",
          headers: { "X-Steward-CSRF": session.value.csrf },
          path: { request_id: envelopeRequest.id },
        }),
      ]);
      if (!exact.data || !exact.response?.ok || !wrapper.data || !wrapper.response?.ok) {
        setFailure("Steward could not render the repository bundle for this exact successful run.");
        setState("error");
        return;
      }
      await navigator.clipboard.writeText(JSON.stringify(
        exactRepositoryBundle(exact.data.files, wrapper.data.files),
        null,
        2,
      ));
      setState("copied");
    } catch {
      setFailure("Steward could not render the repository bundle. Retry the request; if it persists, contact an administrator.");
      setState("error");
    }
  }

  return (
    <section className="mt-5 rounded-card border p-4" aria-labelledby="save-task-title">
      <h3 className="text-sm font-semibold" id="save-task-title">Save this task to a repository</h3>
      <p className="mt-2 text-sm text-muted-ink">Copy the exact package that succeeded plus its GitHub Actions caller workflow. Steward never rebuilds the package from the form.</p>
      <label className="mt-4 grid gap-2 text-sm font-semibold">Target repository<input className="min-h-11 w-full rounded-control border bg-panel px-3 font-mono font-normal" onChange={(event) => setRepository(event.target.value)} value={repository} /></label>
      <button className="mt-4 rounded-control border bg-panel px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={state === "working" || !repository.trim()} onClick={() => void copyBundle()} type="button">{state === "working" ? "Rendering…" : state === "copied" ? "Bundle copied" : "Copy repository bundle"}</button>
      {failure ? <p className="mt-3 text-sm text-err" role="alert">{failure}</p> : null}
    </section>
  );
}

export function RunCards({ admin = false, runs }: Readonly<{ admin?: boolean; runs: Array<BrowserRunView> }>) {
  if (runs.length === 0) return <p className="rounded-card border bg-panel p-6 text-sm text-muted-ink">No runs yet.</p>;
  const newestRuns = [...runs].sort((left, right) => runUpdatedAt(right) - runUpdatedAt(left));
  const columns = [
    { key: "workflow", label: "Workflow", className: "font-semibold", render: (run: BrowserRunView) => <span><span className="block truncate">{run.workflow}</span><span className="mt-0.5 block truncate font-mono text-xs font-normal text-muted-ink">{run.taskUid}</span></span> },
    ...(admin ? [{ key: "owner", label: "Owner", render: (run: BrowserRunView) => <span className="block truncate text-muted-ink">{"ownerDisplayEmail" in run ? String(run.ownerDisplayEmail ?? "Not reported") : "Not reported"}</span> }] : []),
    { key: "source", label: "Origin / package", render: (run: BrowserRunView) => <span><span className="block">{run.origin}</span><span className="mt-0.5 block max-w-64 truncate font-mono text-xs text-muted-ink">{run.package ? `${run.package.source} @ ${run.package.revision}` : "No package pin"}</span></span> },
    { key: "envelope", label: "Envelope", className: "font-mono text-xs text-muted-ink", render: (run: BrowserRunView) => run.userEnvelopeRevision ? `rev ${run.userEnvelopeRevision}` : "—" },
    { key: "status", label: "Status", render: (run: BrowserRunView) => <StatusBadge value={run.phase} /> },
    { key: "runtime", label: "Runtime", className: "font-mono text-xs text-muted-ink", render: (run: BrowserRunView) => <span className={run.runtimeUid ? "text-ink" : "text-faint-ink"}>{runtimeLabel(run.runtimeUid)}</span> },
    { key: "spend", label: "Spend", className: "text-right tabular-nums", render: (run: BrowserRunView) => run.observedSpend ? `${run.observedSpend.observedAmount} ${run.observedSpend.currency}` : "—" },
    { key: "updated", label: "Updated", className: "text-right text-muted-ink", render: (run: BrowserRunView) => relativeTime(run.updatedAt) },
  ];
  return <DataTable
    ariaLabel={admin ? "All runs" : "Runs"}
    columns={columns}
    gridTemplateColumns={admin ? "minmax(180px,1.2fr) minmax(160px,1fr) minmax(220px,1.4fr) 90px 110px 110px 90px 150px" : "minmax(190px,1.2fr) minmax(220px,1.4fr) 90px 110px 110px 90px 150px"}
    minWidth={admin ? "1280px" : "1100px"}
    rowHref={(run) => `${admin ? "/admin/runs" : "/runs"}/${run.taskUid}`}
    rowKey={(run) => run.taskUid}
    rows={newestRuns}
  />;
}

export function RunsView({ admin = false }: Readonly<{ admin?: boolean }>) {
  const [phase, setPhase] = useState("all");
  const load = useCallback(() => admin
    ? loadAllRuns() as Promise<{ data?: AllRunsResponse; response?: Response }>
    : loadAllMyRuns() as Promise<{ data?: MyRunsResponse; response?: Response }>, [admin]);
  const state = useApiResource<AllRunsResponse | MyRunsResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader
        actions={!admin ? <Link className="rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand" href="/runs/new">Run now</Link> : null}
        description={admin ? "Every governed run across all users." : "Agent runs executed under your envelopes."}
        title={admin ? "All runs" : "Runs"}
      />
      <ResourceBoundary state={state}>{(data) => (
        <div className="space-y-5">
          <FilterChips
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
  const search = useSearchParams();
  const session = useSession();
  const [cancelState, setCancelState] = useState<"idle" | "working" | "cancelled" | MutationFailureState>("idle");
  const [cancelOpen, setCancelOpen] = useState(false);
  const [rerunState, setRerunState] = useState<"idle" | "working" | ConnectionMutationState>("idle");
  const [liveRun, setLiveRun] = useState<BrowserRunResponse | null>(null);
  const [liveTimeline, setLiveTimeline] = useState<BrowserRunTimelineResponse | null>(null);
  const loadRun = useCallback(() => admin
    ? allRun({ cache: "no-store", credentials: "same-origin", path: { task_uid: taskUid } })
    : myRun({ cache: "no-store", credentials: "same-origin", path: { task_uid: taskUid } }), [admin, taskUid]);
  const loadTimeline = useCallback(() => admin
    ? allRunTimeline({ cache: "no-store", credentials: "same-origin", path: { task_uid: taskUid } })
    : myRunTimeline({ cache: "no-store", credentials: "same-origin", path: { task_uid: taskUid } }), [admin, taskUid]);
  const runState = useApiResource<BrowserRunResponse>(loadRun);
  const timelineState = useApiResource<BrowserRunTimelineResponse>(loadTimeline);
  useEffect(() => {
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let pollingDelay = 2_000;
    let eventSource: EventSource | undefined;

    function schedule(next: () => Promise<void>, delay: number) {
      if (active) timer = setTimeout(() => void next(), delay);
    }

    function apply(run: BrowserRunResponse, timeline: BrowserRunTimelineResponse) {
      if (!active) return true;
      setLiveRun(run);
      setLiveTimeline(timeline);
      return isTerminalPhase(run.run.phase) && run.run.finalized;
    }

    async function poll() {
      try {
        const [run, timeline] = await Promise.all([loadRun(), loadTimeline()]);
        if (run.data && timeline.data && run.response?.ok && timeline.response?.ok) {
          pollingDelay = 2_000;
          if (!apply(run.data, timeline.data)) schedule(poll, pollingDelay);
          return;
        }
      } catch {
        // The bounded polling fallback retries below.
      }
      pollingDelay = Math.min(pollingDelay * 2, 10_000);
      schedule(poll, pollingDelay);
    }

    function applyEvent(event: MessageEvent<string>) {
      const snapshot = parseRunEventData(event.data);
      if (!snapshot) return;
      if (apply({ apiVersion: "steward.browser-runs/v1", run: snapshot.run }, snapshot.timeline)) {
        eventSource?.close();
      }
    }

    if (admin) void poll();
    else {
      eventSource = new EventSource(`/app/api/v1/runs/${encodeURIComponent(taskUid)}/events`);
      for (const eventName of ["snapshot", "run", "job", "output", "log"]) {
        eventSource.addEventListener(eventName, applyEvent as EventListener);
      }
    }
    return () => {
      active = false;
      if (timer) clearTimeout(timer);
      eventSource?.close();
    };
  }, [admin, loadRun, loadTimeline, taskUid]);

  function selectLocation(stageId: string, stream: ExecutionLogStream | null = null) {
    const parameters = new URLSearchParams(search.toString());
    parameters.set("job", stageId);
    if (stream) parameters.set("stream", stream);
    else parameters.delete("stream");
    router.push(`${admin ? "/admin/runs" : "/runs"}/${encodeURIComponent(taskUid)}?${parameters.toString()}`, { scroll: false });
  }
  async function cancelRun() {
    if (session.status !== "authenticated") return;
    setCancelState("working");
    const result = await cancelMyRun({ cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { task_uid: taskUid } });
    if (result.response?.status === 409) {
      window.location.reload();
      return;
    }
    setCancelState(result.data && result.response?.ok ? "cancelled" : classifyMutationFailure(result.response?.status));
  }
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <ResourceBoundary state={runState}>{(initial) => {
        const { run } = liveRun ?? initial;
        const pinnedWorkflow = run.workflowName && run.workflowVersion
          ? `${run.workflowName}@${run.workflowVersion}`
          : run.workflow;
        const requestedStageId = search.get("job");
        const requestedStream = search.get("stream");
        const selectedStream = requestedStream === "stdout" || requestedStream === "stderr" ? requestedStream : null;
        const selectedStage = run.stages.find((stage) => stage.id === requestedStageId) ?? run.stages.find((stage) => stage.state === "running" || stage.state === "failed") ?? run.stages[0];
        const timelineEvents = liveTimeline?.events ?? (timelineState.status === "ready" ? timelineState.value.events : []);
        const admitted = timelineEvents.find((event) => event.kind === "admitted");
        const runtimeBound = timelineEvents.find((event) => event.kind === "runtimeBound");
        return (
          <article className="space-y-6">
            <header className="flex flex-wrap items-start justify-between gap-5">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-3"><h1 className="text-[28px] font-semibold tracking-tight" id="page-title">{pinnedWorkflow}</h1><StatusBadge value={run.phase} /></div>
                <p className="mt-1 break-all font-mono text-xs text-muted-ink">{run.taskUid}</p>
                {run.trigger ? <div className="mt-2 text-sm text-muted-ink">Triggered by <strong className="font-medium text-ink">{run.trigger.actor}</strong> via {run.trigger.event} · <a href={run.trigger.runUrl} rel="noreferrer" target="_blank">{run.trigger.repository}@{run.trigger.ref} ({run.trigger.sha.slice(0, 7)})</a> · {durationLabel(run.createdAt, run.updatedAt)}</div> : <div className="mt-2 text-sm text-muted-ink">Started {dateTime(run.createdAt)} · {durationLabel(run.createdAt, run.updatedAt)}</div>}
                <div className="mt-1 text-sm text-muted-ink">Origin: {run.origin}{run.package ? ` · Package: ${run.package.source} @ ${run.package.revision} · ${run.package.path} · Prompt: ${run.package.promptSource}` : ""}</div>
              </div>
              {!admin ? <div className="flex flex-wrap gap-2"><button className="min-h-10 rounded-control border bg-panel px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={rerunState === "working" || !run.rerunSupported} title={run.rerunSupported ? undefined : run.rerunUnavailableReason ?? "This run cannot be re-run."} onClick={async () => {
                if (session.status !== "authenticated") return;
                setRerunState("working");
                const idempotencyKey = crypto.randomUUID();
                const outcome = await pollRerun(() => rerunMyRun({ body: { idempotencyKey }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { task_uid: taskUid } }));
                if ("taskUid" in outcome) router.push(`/runs/${outcome.taskUid}`);
                else setRerunState(outcome.failure);
              }} type="button">{rerunState === "working" ? "Starting…" : "Re-run"}</button>{!isTerminalPhase(run.phase) ? <details className="relative"><summary aria-label="More run actions" className="grid min-h-10 min-w-10 cursor-pointer list-none place-items-center rounded-control border bg-panel px-3 text-lg font-semibold">···</summary><div className="absolute right-0 z-10 mt-1 min-w-40 rounded-control border bg-panel p-1 shadow-lg"><button className="w-full rounded-control px-3 py-2 text-left text-sm font-semibold text-err hover:bg-err-soft" disabled={cancelState === "working" || cancelState === "cancelled"} onClick={() => setCancelOpen(true)} type="button">{cancelState === "working" ? "Cancelling…" : cancelState === "cancelled" ? "Cancellation requested" : "Cancel run"}</button></div></details> : null}</div> : null}
            </header>
            {rerunState !== "idle" && rerunState !== "working" ? <p className="text-sm text-err" role="alert">{rerunFailureMessage(rerunState)}</p> : null}
            {cancelState !== "idle" && cancelState !== "working" && cancelState !== "cancelled" ? <p className="text-sm text-err" role="alert">The run could not be cancelled ({cancelState}).</p> : null}
            <ConfirmationDialog cancelLabel="Keep running" confirmLabel="Cancel run" description="The agent will stop and its runtime credentials will be revoked. This cannot be undone." onConfirm={() => void cancelRun()} onOpenChange={setCancelOpen} open={cancelOpen} pending={cancelState === "working"} title="Cancel this run?" />
            <div className="grid min-h-[540px] overflow-hidden rounded-panel border bg-panel lg:grid-cols-[260px_minmax(0,1fr)]">
              <nav aria-label="Run jobs" className="border-b border-line lg:border-b-0 lg:border-r">
                <div className="border-b border-line-soft px-5 py-4 text-sm font-semibold">Jobs</div>
                {run.stages.length ? <ol className="p-2">{run.stages.map((stage) => <li key={stage.id}><button aria-current={stage.id === selectedStage?.id ? "true" : undefined} className={`flex w-full items-center gap-3 rounded-control px-3 py-3 text-left text-sm ${stage.id === selectedStage?.id ? "bg-brand-soft" : "hover:bg-subtle"}`} onClick={() => selectLocation(stage.id)} type="button"><span aria-hidden="true" className={`size-2.5 shrink-0 rounded-full ${stage.state === "succeeded" ? "bg-ok" : stage.state === "failed" ? "bg-err" : stage.state === "running" ? "bg-info" : "bg-line"}`} /><span className="min-w-0 flex-1 font-medium">{stage.displayName}</span><span className="font-mono text-xs text-faint-ink">{stageDuration(stage.id, run.createdAt, timelineEvents)}</span></button></li>)}</ol> : <div className="p-5"><EmptyState title="No stages reported" /></div>}
                <div className="mx-5 border-t border-line-soft py-4 text-xs text-muted-ink"><p className="font-mono">{taskUid}</p><p className="mt-2">Created {dateTime(run.createdAt)}</p></div>
              </nav>
              <main className="min-w-0">
                <div className="grid border-b border-line-soft bg-subtle sm:grid-cols-2 xl:grid-cols-5">{[
                  ["Workflow version", pinnedWorkflow],
                  ["User envelope revision", admitted?.kind === "admitted" ? admitted.envelopeRevision ?? "—" : run.userEnvelopeRevision ?? "—"],
                  ["Runtime", runtimeBound?.kind === "runtimeBound" ? runtimeBound.runtimeUid : run.runtimeUid ?? "unassigned"],
                  ["Spend", run.observedSpend ? `${run.observedSpend.observedAmount} ${run.observedSpend.currency}` : "—"],
                  ["Agent", run.codingAgentRuntime],
                ].map(([label, value]) => <div className="min-w-0 border-b border-line-soft px-4 py-3 last:border-b-0 sm:border-r xl:border-b-0" key={label}><p className="text-xs text-muted-ink">{label}</p><p className="mt-1 truncate font-mono text-sm font-medium">{value}</p></div>)}</div>
                <section aria-labelledby="selected-stage" className="p-5">
                  <div className="flex items-center justify-between gap-4"><h2 className="text-lg font-semibold" id="selected-stage">{selectedStage?.displayName ?? "Run stages"}</h2>{selectedStage ? <StatusBadge value={selectedStage.state} /> : null}</div>
                  <p className="mt-5 rounded-control border border-warn/30 bg-warn-soft px-4 py-3 text-sm text-warn"><strong className="block font-semibold text-warn" id="sensitivity-notice">Sensitivity notice</strong>Execution logs may reproduce arbitrary user, tool, or agent output.</p>
                  {selectedStage?.id === "agent_execution" && run.executionLog === "off" ? <div className="mt-5 rounded-card border p-4"><p className="text-sm font-semibold">No execution log was captured for this run.</p><p className="mt-1 text-sm text-muted-ink">Enable <strong>Capture execution log</strong> when starting a run to retain stdout and stderr. Captured output may contain sensitive data.</p></div> : null}
                  {selectedStage?.steps.length ? <ol className="mt-5 divide-y divide-line-soft rounded-card border">{selectedStage.steps.map((step) => <RunStepRow admin={admin} key={step.id} onStreamChange={(stream) => selectLocation(selectedStage.id, stream)} selectedStream={selectedStage.id === "agent_execution" ? selectedStream : null} step={step} taskUid={taskUid} />)}</ol> : selectedStage ? <p className="mt-5 rounded-card border p-4 text-sm text-muted-ink">{stageSummary(selectedStage)}</p> : null}
                  {!admin && run.phase === "succeeded" && run.finalized ? <RunOutputs taskUid={taskUid} /> : null}
                  {!admin && run.phase === "succeeded" && run.finalized && run.origin === "browser" && run.package?.source === "inline" ? <InlinePackageViewer taskUid={taskUid} /> : null}
                  {!admin && run.phase === "succeeded" && run.finalized && run.origin === "browser" && run.package?.source === "inline" ? <SaveInlineRunToRepository run={run} /> : null}
                </section>
              </main>
            </div>
          </article>
        );
      }}</ResourceBoundary>
    </section>
  );
}
