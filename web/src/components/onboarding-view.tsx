"use client";

import Link from "next/link";
import { useCallback, useEffect, useMemo, useState, type FormEvent, type ReactNode } from "react";

import {
  detectWorkflow,
  dispatchTask,
  getStarterTask,
  githubAutomationEvidence,
  githubRunStatus,
  githubTaskBundle,
  listPublishedWorkflows,
  listTemplates,
  myRunExecutionLog,
  publishTask,
  submitBrowserRun,
  updateBrowserPreferences,
  type BrowserRunView,
  type DispatchTaskResponse,
  type EnvelopeTemplatesResponse,
  type GithubAutomationEvidenceResponse,
  type GithubAutomationErrorResponse,
  type GithubRunStatusResponse,
  type GithubTaskBundleResponse,
  type PublishTaskResponse,
  type PublishedWorkflowsResponse,
  type StarterTaskSetting,
  type WorkflowDetectionResponse,
} from "@/api-client";
import {
  compatibleAgents,
  effectiveAgentSelection,
  effectiveModelSelection,
  inlineFiles,
  runNowFailureMessage,
} from "@/components/browser-run-now-view";
import { parseRunEventData } from "@/components/run-views";
import { PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { RepositoryResource, useGithubRepositories, type RepositoryResourceState } from "@/data/github-repositories";
import { defaultOnboardingRepository, deriveOnboardingProgress, loadOnboardingEvidence, ONBOARDING_PROGRESS_EVENT, type OnboardingEvidence } from "@/data/onboarding-progress";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

type OnboardingData = OnboardingEvidence & {
  templates: EnvelopeTemplatesResponse;
  starterTask: StarterTaskSetting;
  workflows: PublishedWorkflowsResponse;
};

type InputRow = { id: string; name: string; value: string };

const fieldClass = "min-h-11 w-full rounded-control border bg-panel px-3 font-normal";

function runDuration(createdAt: string, updatedAt: string): string {
  const start = new Date(createdAt).valueOf();
  const end = new Date(updatedAt).valueOf();
  if (!Number.isFinite(start) || !Number.isFinite(end) || end < start) return "duration unavailable";
  const seconds = Math.round((end - start) / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const remainder = seconds % 60;
  return remainder ? `${minutes}m ${remainder}s` : `${minutes}m`;
}

function inputRows(value: unknown): InputRow[] {
  if (!value || typeof value !== "object" || Array.isArray(value)) return [];
  return Object.entries(value as Record<string, unknown>).map(([name, entry], index) => ({
    id: `starter-${index}`,
    name,
    value: typeof entry === "string" ? entry : JSON.stringify(entry),
  }));
}

function inputObject(rows: InputRow[]): Record<string, string> {
  return Object.fromEntries(rows.filter((row) => row.name.trim()).map((row) => [row.name.trim(), row.value]));
}

export function governedJobOnly(workflow: string): string {
  const lines = workflow.split("\n");
  const start = lines.findIndex((line) => line === "  prepare:");
  if (start === -1) return workflow;
  const governed = lines.findIndex((line, index) => index > start && line === "  governed:");
  if (governed === -1) return workflow;
  const endOffset = lines.slice(governed + 1).findIndex((line) => /^  [A-Za-z0-9_-]+:$/.test(line));
  const end = endOffset === -1 ? lines.length : governed + 1 + endOffset;
  return ["jobs:", ...lines.slice(start, end)].join("\n").trimEnd() + "\n";
}

function copyText(value: string, onCopied: () => void) {
  void navigator.clipboard.writeText(value).then(onCopied);
}

function repositoryPrerequisite(reason: string | null | undefined): string {
  if (reason === "source_repository_not_admitted") return "This repository is not admitted as a governed source.";
  return reason ? `Repository prerequisite is missing: ${reason}.` : "This repository is not ready for governed automation.";
}

export { browserHelloWorldRun } from "@/data/onboarding-progress";

export function OnboardingView() {
  const [refresh, setRefresh] = useState(0);
  const repositories = useGithubRepositories();
  const load = useCallback(async () => {
    void refresh;
    const [evidence, templates, starterTask, workflows] = await Promise.all([
      loadOnboardingEvidence(),
      listTemplates({ cache: "no-store", credentials: "same-origin" }),
      getStarterTask({ cache: "no-store", credentials: "same-origin" }),
      listPublishedWorkflows({ cache: "no-store", credentials: "same-origin" }),
    ]);
    const required = [evidence, templates, starterTask, workflows];
    const data = evidence.data && templates.data && starterTask.data && workflows.data
      ? {
          ...evidence.data,
          templates: templates.data,
          starterTask: starterTask.data.starterTask,
          workflows: workflows.data,
        }
      : undefined;
    return { data, response: required.find((result) => !result.response?.ok)?.response ?? evidence.response };
  }, [refresh]);
  const state = useApiResource<OnboardingData>(load);
  const handleRefresh = useCallback(() => setRefresh((value) => value + 1), []);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <ResourceBoundary state={state}>{(data) => <OnboardingChecklist data={data} onRefresh={handleRefresh} onRetryRepositories={repositories.retry} repositories={repositories.state} />}</ResourceBoundary>
    </section>
  );
}

function OnboardingChecklist({ data, onRefresh, onRetryRepositories, repositories }: Readonly<{
  data: OnboardingData;
  onRefresh: () => void;
  onRetryRepositories: () => void;
  repositories: RepositoryResourceState;
}>) {
  const session = useSession();
  const baseProgress = useMemo(() => deriveOnboardingProgress(data), [data]);
  const activeEnvelopes = data.envelopes.requests.filter((request) => request.status === "provisioned" && request.envelopeDigest);
  const connectedConnection = data.connections.connections.find((connection) => connection.status.phase === "connected");
  const provisionedRequest = activeEnvelopes[0];
  const repositoryData = repositories.status === "ready" ? repositories.value : null;
  const repositoryList = useMemo(() => repositoryData?.repositories ?? [], [repositoryData]);
  const readyRepository = defaultOnboardingRepository(repositoryList);
  const [selectedTemplateId, setSelectedTemplateId] = useState(data.templates.templates[0]?.id ?? "");
  const [selectedEnvelopeId, setSelectedEnvelopeId] = useState(provisionedRequest?.id ?? "");
  const [selectedRepositoryId, setSelectedRepositoryId] = useState(readyRepository?.repositoryId ?? "");
  const [agentRef, setAgentRef] = useState(data.starterTask.taskDefinition.runtime.agentRef);
  const [prompt, setPrompt] = useState(data.starterTask.taskDefinition.promptText ?? "");
  const [inputs, setInputs] = useState<InputRow[]>(() => inputRows(data.starterTask.inputs));
  const [createdRunUid, setCreatedRunUid] = useState<string | null>(null);
  const [runState, setRunState] = useState<"idle" | "submitting" | "error">("idle");
  const [runFailure, setRunFailure] = useState<string | null>(null);
  const [bundleResource, setBundleResource] = useState<{
    bundle: GithubTaskBundleResponse | null;
    failure: string | null;
    manualFiles: Record<string, string>;
    taskUid: string;
  } | null>(null);
  const [publication, setPublication] = useState<PublishTaskResponse | null>(null);
  const [workflow, setWorkflow] = useState<WorkflowDetectionResponse | null>(null);
  const [dispatch, setDispatch] = useState<DispatchTaskResponse | null>(null);
  const [githubStatus, setGithubStatus] = useState<GithubRunStatusResponse | null>(null);
  const [automationState, setAutomationState] = useState<"idle" | "checking" | "publishing" | "dispatching" | "error">("idle");
  const [automationFailure, setAutomationFailure] = useState<string | null>(null);
  const [dismissal, setDismissal] = useState<"idle" | "working" | "done" | "shown" | "error">("idle");
  const [copied, setCopied] = useState<string | null>(null);
  const [workflowTab, setWorkflowTab] = useState<"full" | "job">("full");

  const selectedEnvelope = activeEnvelopes.find((request) => request.id === selectedEnvelopeId) ?? activeEnvelopes[0];
  const selectedRepository = repositoryList.find((repository) => repository.repositoryId === selectedRepositoryId) ?? readyRepository;
  const envelope = selectedEnvelope?.approvedEnvelope ?? selectedEnvelope?.requestedEnvelope;
  const agentOptions = compatibleAgents(data.workflows.agents.map((agent) => agent.agentRef), envelope?.spec.llms ?? []);
  const { selected: selectedAgent, warning: agentWarning } = effectiveAgentSelection(agentOptions, agentRef);
  const { selected: selectedModel, warning: modelWarning } = effectiveModelSelection(
    data.starterTask.taskDefinition.runtime.model ?? null,
    data.starterTask.taskDefinition.runtime.agentRef,
    selectedAgent,
    envelope?.spec.llms ?? [],
  );
  const effectiveFiles = selectedAgent && selectedModel
    ? inlineFiles(
        { ...data.starterTask, taskDefinition: { ...data.starterTask.taskDefinition, promptText: prompt } },
        selectedAgent.agentRef,
        selectedModel,
      )
    : null;
  const testRun = baseProgress.helloWorldRun;
  const successfulTestRun = testRun?.phase === "succeeded" && testRun.finalized ? testRun : undefined;
  const taskUid = successfulTestRun?.taskUid ?? null;
  const packagePath = successfulTestRun?.package?.path ?? data.starterTask.packagePath;
  const currentBundleResource = bundleResource?.taskUid === taskUid ? bundleResource : null;
  const bundle = currentBundleResource?.bundle ?? null;
  const manualFiles = currentBundleResource?.manualFiles ?? {};
  const bundleFailure = currentBundleResource?.failure ?? null;
  const packagePreview = bundle?.files[packagePath] ?? manualFiles[packagePath] ?? null;
  const workflowPreview = bundle ? bundle.files[bundle.workflowPath] ?? null : null;
  const automationRun = baseProgress.automationRun;
  const automationRunId = Number(automationRun?.trigger?.runId);
  const statusRunId = dispatch?.runId
    ?? (Number.isSafeInteger(automationRunId) && automationRunId > 0 ? automationRunId : null);
  const githubRunUrl = dispatch?.url ?? automationRun?.trigger?.runUrl ?? null;
  const resultPhase = automationRun?.phase ?? githubStatus?.linkedTaskPhase ?? githubStatus?.conclusion ?? githubStatus?.phase;
  const progress = deriveOnboardingProgress(data, {
    dispatchObserved: Boolean(dispatch),
    publicationObserved: Boolean(publication || workflow?.compatible),
    workflowObserved: Boolean(workflow?.compatible),
  });
  const done = progress.done;
  const completed = progress.completed;
  const firstIncomplete = done.findIndex((value) => !value);
  const [openStep, setOpenStep] = useState(firstIncomplete === -1 ? 6 : firstIncomplete);

  useEffect(() => {
    window.dispatchEvent(new CustomEvent(ONBOARDING_PROGRESS_EVENT, { detail: progress }));
  }, [progress]);

  const refreshWorkflow = useCallback(async () => {
    if (session.status !== "authenticated" || !taskUid || !selectedRepository) return null;
    const result = await detectWorkflow({
      body: { owner: selectedRepository.owner, repository: selectedRepository.name },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
      path: { task_uid: taskUid },
    });
    const detected = result.data && result.response?.ok ? result.data : null;
    setWorkflow(detected);
    return detected;
  }, [selectedRepository, session, taskUid]);

  useEffect(() => {
    let active = true;
    if (session.status !== "authenticated" || !taskUid) return () => { active = false; };
    void githubTaskBundle({
        cache: "no-store",
        credentials: "same-origin",
        path: { task_uid: taskUid },
      }).then((bundleResult) => {
      if (!active) return;
      const problem = bundleResult.error as GithubAutomationErrorResponse | undefined;
      setBundleResource({
        bundle: bundleResult.data && bundleResult.response?.ok ? bundleResult.data : null,
        failure: bundleResult.data && bundleResult.response?.ok
          ? null
          : problem?.error === "steward_run_release_unsupported"
            ? "The reviewed steward-run release is below 0.8.0. Copy the Task definition manually; workflow generation and detection require steward-run 0.8.0 or later."
            : "Steward could not render the exact tested package and workflow.",
        manualFiles: problem?.manualFiles ?? {},
        taskUid,
      });
    }).catch(() => {
      if (active) setBundleResource({ bundle: null, failure: "Steward could not render the exact tested package and workflow.", manualFiles: {}, taskUid });
    });
    return () => { active = false; };
  }, [session.status, taskUid]);

  useEffect(() => {
    let active = true;
    void (async () => {
      await Promise.resolve();
      if (!active) return;
      setPublication(null);
      setWorkflow(null);
      setDispatch(null);
      setGithubStatus(null);
      if (session.status !== "authenticated" || !taskUid || !selectedRepository) return;
      setAutomationState("checking");
      const [evidenceResult] = await Promise.all([
        selectedRepository.ready ? githubAutomationEvidence({
          cache: "no-store",
          credentials: "same-origin",
          path: { task_uid: taskUid },
          query: { owner: selectedRepository.owner, repository: selectedRepository.name },
        }) : Promise.resolve(null),
        refreshWorkflow(),
      ]);
      if (!active) return;
      if (evidenceResult?.data && evidenceResult.response?.ok) {
        const evidence: GithubAutomationEvidenceResponse = evidenceResult.data;
        setPublication(evidence.publication ?? null);
        setDispatch(evidence.dispatch ?? null);
      }
      setAutomationState("idle");
    })();
    return () => { active = false; };
  }, [refreshWorkflow, selectedRepository, session, taskUid]);

  const readinessFailure = selectedRepository && !selectedRepository.ready
    ? repositoryPrerequisite(selectedRepository.missingPrerequisite)
    : null;

  useEffect(() => {
    if (!publication || workflow?.compatible || !selectedRepository?.ready) return;
    const timer = window.setInterval(() => void refreshWorkflow(), 5_000);
    return () => window.clearInterval(timer);
  }, [publication, refreshWorkflow, selectedRepository?.ready, workflow?.compatible]);

  useEffect(() => {
    if (!statusRunId || !taskUid || !selectedRepository || githubStatus?.phase === "completed") return;
    let active = true;
    async function refreshStatus() {
      const result = await githubRunStatus({
        cache: "no-store",
        credentials: "same-origin",
        path: { task_uid: taskUid!, run_id: statusRunId! },
        query: { owner: selectedRepository!.owner, repository: selectedRepository!.name },
      });
      if (active && result.data && result.response?.ok) {
        setGithubStatus(result.data);
        if (!automationRun && result.data.linkedTaskPhase && ["succeeded", "failed", "cancelled"].includes(result.data.linkedTaskPhase)) onRefresh();
      }
    }
    void refreshStatus();
    const timer = window.setInterval(() => void refreshStatus(), 5_000);
    return () => { active = false; window.clearInterval(timer); };
  }, [automationRun, githubStatus?.phase, onRefresh, selectedRepository, statusRunId, taskUid]);

  async function updatePreferences(body: { onboardingDismissed?: boolean }) {
    if (session.status !== "authenticated") return false;
    const result = await updateBrowserPreferences({ body, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf } });
    const accepted = Boolean(result.data && result.response?.ok);
    if (accepted && typeof body.onboardingDismissed === "boolean") {
      window.dispatchEvent(new CustomEvent("hypershell:preferences-updated", { detail: { onboardingDismissed: body.onboardingDismissed } }));
    }
    return accepted;
  }

  function hideGuide() {
    setDismissal("done");
    window.dispatchEvent(new CustomEvent("hypershell:preferences-updated", { detail: { onboardingDismissed: true } }));
    void updatePreferences({ onboardingDismissed: true }).then((accepted) => {
      if (!accepted) {
        window.dispatchEvent(new CustomEvent("hypershell:preferences-updated", { detail: { onboardingDismissed: false } }));
        setDismissal("error");
      }
    });
  }

  async function submitTestRun(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (session.status !== "authenticated" || !selectedEnvelope?.envelopeDigest || !effectiveFiles) return;
    setRunState("submitting");
    setRunFailure(null);
    const envelopeDigest = selectedEnvelope.envelopeDigest.startsWith("steward:")
      ? selectedEnvelope.envelopeDigest
      : `steward:${selectedEnvelope.envelopeDigest}`;
    const result = await submitBrowserRun({
      body: {
        package: { source: "inline", path: data.starterTask.packagePath, files: effectiveFiles },
        envelopeDigest,
        inputs: inputObject(inputs),
        diagnostics: { executionLog: data.starterTask.executionLog },
      },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "Idempotency-Key": crypto.randomUUID(), "X-Steward-CSRF": session.value.csrf },
    });
    if (result.data && result.response?.ok) {
      setCreatedRunUid(result.data.taskUid);
      setRunState("idle");
      return;
    }
    setRunFailure(runNowFailureMessage(result.error));
    setRunState("error");
  }

  async function openPublication() {
    if (session.status !== "authenticated" || !taskUid || !selectedRepository?.ready || !bundle) return;
    setAutomationState("publishing");
    setAutomationFailure(null);
    const result = await publishTask({
      body: { owner: selectedRepository.owner, repository: selectedRepository.name, idempotencyKey: crypto.randomUUID() },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
      path: { task_uid: taskUid },
    });
    if (result.data && result.response?.ok) {
      setPublication(result.data);
      setAutomationState("idle");
      return;
    }
    setAutomationFailure("Steward could not open the publication pull request.");
    setAutomationState("error");
  }

  async function dispatchWorkflow() {
    if (session.status !== "authenticated" || !taskUid || !selectedRepository || !workflow?.compatible) return;
    setAutomationState("dispatching");
    setAutomationFailure(null);
    const taskInputs = JSON.stringify(inputObject(inputs));
    const result = await dispatchTask({
      body: {
        owner: selectedRepository.owner,
        repository: selectedRepository.name,
        inputs: { "task-inputs": taskInputs },
        idempotencyKey: crypto.randomUUID(),
      },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
      path: { task_uid: taskUid },
    });
    if (result.data && result.response?.ok) {
      setDispatch(result.data);
      setGithubStatus(null);
      setAutomationState("idle");
      return;
    }
    setAutomationFailure("Steward could not dispatch the exact published workflow.");
    setAutomationState("error");
  }

  const header = <PageHeader
    actions={<button aria-label="Hide Get started" className="grid size-10 place-items-center rounded-control border text-lg text-muted-ink hover:bg-line-soft hover:text-ink" disabled={dismissal === "working" || dismissal === "done"} onClick={hideGuide} type="button"><span aria-hidden="true">×</span></button>}
    description="Seven steps from a first test run to a governed GitHub workflow. An envelope is the budget, models and tools an agent may use; the workflow runs your agent inside it."
    title="Get started"
  />;

  if ((data.preferences.onboardingDismissed && dismissal !== "shown") || dismissal === "done") {
    return <>{header}<div className="rounded-card border bg-panel p-5"><p className="text-sm text-muted-ink">The onboarding guide is hidden. Your progress is preserved.</p><button className="mt-4 rounded-control border px-4 py-2 text-sm font-semibold" onClick={async () => { setDismissal("working"); setDismissal(await updatePreferences({ onboardingDismissed: false }) ? "shown" : "error"); }} type="button">Show guide again</button></div></>;
  }

  const stepRows: Array<{ title: string; status: string; body: ReactNode }> = [
    {
      title: "Connect GitHub",
      status: done[0] ? `Connected as ${repositoryData?.login || connectedConnection?.status.accountEmail || "GitHub user"}` : "Authorize repository access",
      body: <div className="space-y-3"><p className="text-sm text-muted-ink">HyperShell holds the GitHub credential on your behalf. Agents never see it.</p><div className="flex flex-wrap gap-3"><Link className="rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand" href="/connections">Connect GitHub</Link><Link className="self-center text-sm font-semibold" href="/connections">Manage connections</Link></div></div>,
    },
    {
      title: "Get your first envelope",
      status: done[1] ? `${provisionedRequest?.templateId ?? "Custom"} envelope ${provisionedRequest?.envelopeInstanceId} provisioned` : "Choose the authority for this run",
      body: <div className="space-y-4"><p className="text-sm text-muted-ink">Requests within a template&apos;s ceiling are approved and provisioned right away.</p><div className="grid grid-cols-[repeat(auto-fit,minmax(170px,1fr))] gap-3">{data.templates.templates.map((template) => <button aria-pressed={template.id === selectedTemplateId} className={`rounded-tile border p-4 text-left ${template.id === selectedTemplateId ? "border-brand bg-brand-soft" : "hover:bg-subtle"}`} key={template.id} onClick={() => setSelectedTemplateId(template.id)} type="button"><span className="flex justify-between gap-3"><strong className="text-sm">{template.displayName}</strong><span className="font-mono text-xs text-muted-ink">rev {template.revision}</span></span><span className="mt-2 block text-xs text-muted-ink">Up to {template.ceiling.spec.budget.monthlyLimit} {template.ceiling.spec.budget.currency}/mo · {template.ceiling.spec.ttl}</span></button>)}</div><div className="flex flex-wrap gap-3"><Link className="rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand" href={`/envelopes/new?template=${encodeURIComponent(selectedTemplateId)}`}>Request {data.templates.templates.find((template) => template.id === selectedTemplateId)?.displayName ?? "an"} envelope</Link><Link className="self-center text-sm font-semibold" href="/envelopes/new?type=custom">Customize limits instead</Link></div></div>,
    },
    {
      title: "Try a test run",
      status: successfulTestRun ? `Test run succeeded in ${runDuration(successfulTestRun.createdAt, successfulTestRun.updatedAt)} · ${successfulTestRun.observedSpend?.observedAmount ?? "—"} ${successfulTestRun.observedSpend?.currency ?? ""}` : testRun ? `${testRun.phase} · ${testRun.taskUid}` : "Check the combination before wiring it into CI",
      body: <form className="space-y-4" onSubmit={(event) => void submitTestRun(event)}>
        <p className="text-sm text-muted-ink">Check the combination before wiring it into CI. This run is governed exactly like one started from GitHub.</p>
        <div className="grid gap-4 md:grid-cols-3">
          <div className="grid content-start gap-2 text-sm"><strong>Repository</strong><RepositoryResource onRetry={onRetryRepositories} state={repositories}>{({ repositories: available }) => <>{available.length ? <label><span className="sr-only">Repository</span><select aria-label="Repository" className={fieldClass} onChange={(event) => setSelectedRepositoryId(event.target.value)} value={selectedRepository?.repositoryId ?? ""}>{available.map((repository) => <option key={repository.repositoryId} value={repository.repositoryId}>{repository.owner}/{repository.name}{repository.ready ? " · Ready" : " · Not ready"}</option>)}</select></label> : <p className="font-normal text-muted-ink">No GitHub repositories are visible to this connection.</p>}<button className="justify-self-start rounded-control border px-3 py-1.5 text-xs font-semibold" onClick={onRetryRepositories} type="button">Refresh repositories</button></>}</RepositoryResource></div>
          <label className="grid gap-2 text-sm font-semibold">Coding agent<select className={fieldClass} onChange={(event) => setAgentRef(event.target.value)} value={agentRef}>{agentOptions.map((agent) => <option disabled={!agent.compatible} key={agent.agentRef} value={agent.agentRef}>{agent.agentRef}{agent.compatible ? "" : " · unavailable"}</option>)}</select></label>
          <label className="grid gap-2 text-sm font-semibold">Envelope<select className={fieldClass} onChange={(event) => setSelectedEnvelopeId(event.target.value)} value={selectedEnvelope?.id ?? ""}>{activeEnvelopes.map((request) => <option key={request.id} value={request.id}>{request.templateId ?? "Custom"} · rev {request.approvedEnvelope?.revision ?? request.requestedEnvelope.revision}</option>)}</select></label>
        </div>
        {readinessFailure ? <p className="text-sm text-warn">Not ready: {readinessFailure}</p> : null}
        {agentWarning || modelWarning ? <p className="text-sm text-warn">{agentWarning ?? modelWarning}</p> : null}
        <label className="grid gap-2 text-sm font-semibold">Prompt<textarea className="min-h-32 rounded-control border bg-panel p-3 font-mono text-sm font-normal" onChange={(event) => setPrompt(event.target.value)} value={prompt} /></label>
        <div className="space-y-3"><div className="flex items-center justify-between gap-3"><strong className="text-sm">Inputs</strong><button className="rounded-control border px-3 py-1.5 text-sm font-semibold" onClick={() => setInputs((rows) => [...rows, { id: crypto.randomUUID(), name: "", value: "" }])} type="button">+ Add input</button></div>{inputs.map((row) => <div className="grid gap-2 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto]" key={row.id}><input aria-label="Input name" className={fieldClass} onChange={(event) => setInputs((rows) => rows.map((candidate) => candidate.id === row.id ? { ...candidate, name: event.target.value } : candidate))} placeholder="name" value={row.name} /><input aria-label={`Input value for ${row.name || "new input"}`} className={fieldClass} onChange={(event) => setInputs((rows) => rows.map((candidate) => candidate.id === row.id ? { ...candidate, value: event.target.value } : candidate))} placeholder="value" value={row.value} /><button aria-label={`Remove input ${row.name || "row"}`} className="rounded-control border px-3 text-sm" onClick={() => setInputs((rows) => rows.filter((candidate) => candidate.id !== row.id))} type="button">Remove</button></div>)}<p className="text-xs text-muted-ink">Inputs are passed to the agent as <code>in/inputs.json</code>.</p></div>
        <div className="flex flex-wrap items-center gap-3"><button className="rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={runState === "submitting" || !selectedEnvelope || !effectiveFiles} type="submit">{runState === "submitting" ? "Starting…" : "Run test"}</button><span className="text-xs text-muted-ink">Spends from this envelope. Typically under 0.05 USD.</span></div>
        {runFailure ? <p className="text-sm text-err" role="alert">{runFailure}</p> : null}
        {createdRunUid ? <LiveRunCard onTerminal={onRefresh} taskUid={createdRunUid} /> : testRun ? <RunSummary run={testRun} /> : null}
      </form>,
    },
    {
      title: "Publish the task definition",
      status: publication ? `Pull request #${publication.pullRequestNumber} opened` : workflow?.compatible ? "Published workflow found on the default branch" : "Publish the exact tested Task as a pull request",
      body: successfulTestRun ? <div className="space-y-4"><p className="text-sm text-muted-ink">Review the exact <code>steward.task-definition/v2</code> document that ran at <code>{packagePath}</code>.</p>{packagePreview ? <Preview title={packagePath} value={packagePreview} copied={copied === "package"} onCopy={() => copyText(packagePreview, () => setCopied("package"))} /> : bundleFailure ? null : <p className="text-sm text-warn">Rendering the tested package…</p>}{bundleFailure ? <p className="text-sm text-warn">{bundleFailure}</p> : null}<RepositoryResource onRetry={onRetryRepositories} state={repositories}>{({ repositories: available }) => available.length ? null : <p className="text-sm text-muted-ink">The tested package is ready, but no repository is available for publication.</p>}</RepositoryResource>{readinessFailure ? <p className="text-sm text-warn">{readinessFailure} Copy the generated files into the repository manually.</p> : null}<div className="flex flex-wrap gap-3"><button className="rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={!bundle || !selectedRepository?.ready || automationState === "publishing"} onClick={() => void openPublication()} type="button">{automationState === "publishing" ? "Opening…" : "Open pull request"}</button>{publication ? <a className="rounded-control border px-4 py-2 text-sm font-semibold" href={publication.pullRequestUrl} rel="noreferrer" target="_blank">View pull request ↗</a> : null}<button className="rounded-control border px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={!workflowPreview} onClick={() => void refreshWorkflow()} type="button">I&apos;ve committed it</button></div><p className="text-xs text-muted-ink">Change the prompt or inputs in step 3 to regenerate it.</p></div> : <p className="text-sm text-muted-ink">Complete a successful test run first.</p>,
    },
    {
      title: "Add the workflow to your repository",
      status: workflow?.compatible ? `${workflow.path} verified on the default branch` : publication ? "Merge the publication pull request" : "Add the generated caller workflow",
      body: successfulTestRun ? <div className="space-y-4"><div className="flex gap-2" role="tablist" aria-label="Workflow preview"><button aria-selected={workflowTab === "full"} className="rounded-control border px-3 py-2 text-sm font-semibold" onClick={() => setWorkflowTab("full")} role="tab" type="button">Full workflow</button><button aria-selected={workflowTab === "job"} className="rounded-control border px-3 py-2 text-sm font-semibold" onClick={() => setWorkflowTab("job")} role="tab" type="button">Governed job only</button></div>{workflowPreview ? <Preview title={bundle?.workflowPath ?? "Generated workflow"} value={workflowTab === "full" ? workflowPreview : governedJobOnly(workflowPreview)} copied={copied === "workflow"} onCopy={() => copyText(workflowTab === "full" ? workflowPreview : governedJobOnly(workflowPreview), () => setCopied("workflow"))} /> : <p className="text-sm text-warn">{bundleFailure ?? "Rendering the exact workflow…"}</p>}<p className="text-xs text-muted-ink"><code>package-path</code> points to the definition from step 4.</p>{readinessFailure ? <p className="text-sm text-warn">{readinessFailure} Manual workflow detection remains available after you copy the files.</p> : null}<div className="flex flex-wrap gap-3">{publication ? <a className="rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand" href={publication.pullRequestUrl} rel="noreferrer" target="_blank">Merge the pull request ↗</a> : null}<button className="rounded-control border px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={!workflowPreview} onClick={() => void refreshWorkflow()} type="button">I&apos;ve committed it</button></div></div> : <p className="text-sm text-muted-ink">Complete a successful test run first.</p>,
    },
    {
      title: "Trigger a test run",
      status: dispatch ? `GitHub run ${dispatch.runId} accepted` : automationRun ? `GitHub-origin Task ${automationRun.taskUid} observed` : "Dispatch the verified workflow on GitHub",
      body: taskUid && selectedRepository ? <div className="space-y-4"><p className="text-sm text-muted-ink">Dispatch the workflow on GitHub through your connection, or start it yourself. HyperShell picks the run up at admission.</p>{readinessFailure ? <p className="text-sm text-warn">{readinessFailure} Governed dispatch is unavailable until it is ready.</p> : null}{bundleFailure ? <p className="text-sm text-warn">{bundleFailure}</p> : null}<div className="flex flex-wrap gap-3"><button className="rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={!workflow?.compatible || !selectedRepository.ready || automationState === "dispatching"} onClick={() => void dispatchWorkflow()} type="button">{automationState === "dispatching" ? "Starting…" : "Run on GitHub"}</button><a className="rounded-control border px-4 py-2 text-sm font-semibold" href={githubRunUrl ?? `${selectedRepository.url}/actions`} rel="noreferrer" target="_blank">Open in GitHub Actions ↗</a></div>{bundle?.workflowPath ? <pre className="overflow-auto rounded-control bg-code p-3 text-xs text-code-ink">gh workflow run {bundle.workflowPath.split("/").at(-1)} -f task-inputs=&apos;{JSON.stringify(inputObject(inputs))}&apos;</pre> : null}{statusRunId && !githubStatus ? <p className="text-sm text-muted-ink">Waiting for GitHub to start the job…</p> : null}</div> : <p className="text-sm text-muted-ink">Complete the tested package and workflow first.</p>,
    },
    {
      title: "See the result",
      status: resultPhase ? `${resultPhase.charAt(0).toUpperCase()}${resultPhase.slice(1)}${automationRun ? ` in ${runDuration(automationRun.createdAt, automationRun.updatedAt)} · ${automationRun.observedSpend?.observedAmount ?? "—"} ${automationRun.observedSpend?.currency ?? ""}` : ""}` : "The run appears here as soon as GitHub starts it",
      body: dispatch || automationRun ? <div className="grid gap-4 lg:grid-cols-2"><div className="rounded-tile border p-4"><h3 className="text-sm font-semibold">HyperShell run</h3>{automationRun ? <RunSummary linkLabel="Open in HyperShell" run={automationRun} showLogExcerpt /> : githubStatus?.linkedTaskUid ? <div className="mt-3 space-y-2"><StatusBadge value={githubStatus.linkedTaskPhase ?? "queued"} /><p className="font-mono text-xs">{githubStatus.linkedTaskUid}</p><Link className="inline-block font-semibold text-brand" href={`/runs/${githubStatus.linkedTaskUid}`}>Open in HyperShell</Link></div> : <p className="mt-3 text-sm text-muted-ink">The governed Task appears here as soon as GitHub starts it.</p>}</div><div className="rounded-tile border p-4"><h3 className="text-sm font-semibold">GitHub run</h3>{githubStatus ? <div className="mt-3 space-y-3"><div className="flex items-center gap-3"><StatusBadge value={githubStatus.conclusion ?? githubStatus.phase} /><a className="font-semibold text-brand" href={githubStatus.url} rel="noreferrer" target="_blank">Run {githubStatus.runId} ↗</a></div>{githubStatus.jobs.length ? <ul className="space-y-1 text-sm">{githubStatus.jobs.map((job) => <li className="flex justify-between gap-3" key={job.id}><a href={job.url} rel="noreferrer" target="_blank">{job.name}</a><span>{job.conclusion ?? job.status}</span></li>)}</ul> : null}{githubStatus.failureLog ? <pre className="max-h-40 overflow-auto whitespace-pre-wrap rounded-control bg-code p-3 text-xs text-code-ink">{githubStatus.failureLog}</pre> : null}</div> : githubRunUrl ? <a className="mt-3 inline-block font-semibold text-brand" href={githubRunUrl} rel="noreferrer" target="_blank">View on GitHub ↗</a> : null}</div></div> : <p className="text-sm text-muted-ink">The run appears here as soon as GitHub starts it.</p>,
    },
  ];

  return <>{header}<div className="overflow-hidden rounded-card border bg-panel"><div className="flex items-center gap-4 px-5 py-4"><strong className="whitespace-nowrap text-sm">{completed} of 7 done</strong><progress aria-label={`${completed} of 7 onboarding steps complete`} className="h-1.5 flex-1 overflow-hidden rounded-full accent-[var(--color-brand)]" max={7} value={completed} /></div><ol>{stepRows.map((step, index) => <li className="border-t border-line-soft" key={step.title}><button aria-expanded={openStep === index} className="grid w-full grid-cols-[28px_minmax(0,1fr)_auto_14px] items-center gap-3.5 px-5 py-4 text-left hover:bg-subtle" onClick={() => setOpenStep(openStep === index ? -1 : index)} type="button"><span aria-hidden="true" className={`grid size-7 place-items-center rounded-full text-[13px] font-bold ${done[index] ? "bg-ok text-panel" : index === firstIncomplete ? "bg-brand text-on-brand" : "shadow-[inset_0_0_0_1.5px_var(--color-field)] text-muted-ink"}`}>{done[index] ? "✓" : index + 1}</span><span><strong className={`block text-[15px] ${done[index] ? "text-muted-ink" : ""}`}>{step.title}</strong><span className="mt-0.5 block text-[13px] text-muted-ink">{step.status}</span></span>{done[index] ? <span className="rounded-full bg-ok-soft px-2.5 py-1 text-xs font-semibold text-ok">Done</span> : <span />}<span aria-hidden="true" className={`transition-transform ${openStep === index ? "rotate-90" : ""}`}>›</span></button>{openStep === index ? <div className="space-y-3 pb-5 pl-[62px] pr-5">{step.body}</div> : null}</li>)}</ol></div><p className="text-center text-sm text-muted-ink">Hide this guide · you can reopen it from Settings.</p>{automationFailure ? <p className="text-sm text-err" role="alert">{automationFailure}</p> : null}{dismissal === "error" ? <p className="text-sm text-err" role="alert">The onboarding preference could not be updated.</p> : null}</>;
}

function Preview({ copied, onCopy, title, value }: Readonly<{ copied: boolean; onCopy: () => void; title: string; value: string }>) {
  return <div className="rounded-control border bg-subtle"><div className="flex items-center justify-between gap-3 border-b px-3 py-2"><strong className="break-all font-mono text-xs">{title}</strong><button className="rounded-control border bg-panel px-3 py-1.5 text-xs font-semibold" onClick={onCopy} type="button">{copied ? "Copied" : "Copy"}</button></div><pre className="max-h-80 overflow-auto whitespace-pre-wrap p-3 text-xs">{value}</pre></div>;
}

function RunSummary({ linkLabel = "Open run details", run, showLogExcerpt = false }: Readonly<{ linkLabel?: string; run: BrowserRunView; showLogExcerpt?: boolean }>) {
  return <div className="mt-3 space-y-3"><div className="flex flex-wrap items-center gap-3"><StatusBadge value={run.phase} /><span className="text-sm">{run.observedSpend ? `${run.observedSpend.observedAmount} ${run.observedSpend.currency}` : "Spend not reported"}</span></div><p className="font-mono text-xs text-muted-ink">{run.taskUid}</p>{showLogExcerpt ? <RunLogExcerpt run={run} /> : <div className="rounded-control bg-code p-3 font-mono text-xs leading-5 text-code-ink">admission complete<br />runtime provisioned<br />agent execution {run.phase}</div>}<Link className="inline-block font-semibold text-brand" href={`/runs/${run.taskUid}`}>{linkLabel}</Link></div>;
}

function RunLogExcerpt({ run }: Readonly<{ run: BrowserRunView }>) {
  const [excerpt, setExcerpt] = useState<string | null>(null);
  useEffect(() => {
    let active = true;
    if (run.executionLog !== "full") return () => { active = false; };
    void myRunExecutionLog({
      cache: "no-store",
      credentials: "same-origin",
      headers: { Accept: "application/json" },
      path: { task_uid: run.taskUid, stream: "stdout" },
      query: { after: 0 },
    }).then((result) => {
      if (active) setExcerpt(result.data?.content.slice(0, 1_200) || "");
    });
    return () => { active = false; };
  }, [run.executionLog, run.taskUid]);
  if (run.executionLog !== "full") return <p className="text-xs text-muted-ink">No execution log was captured for this run.</p>;
  if (excerpt === null) return <p className="text-xs text-muted-ink">Loading execution log excerpt…</p>;
  return <div className="space-y-1"><pre className="max-h-40 overflow-auto whitespace-pre-wrap rounded-control bg-code p-3 text-xs text-code-ink">{excerpt || "Execution log is empty."}</pre><p className="text-[11px] text-muted-ink">Captured output may contain sensitive data.</p></div>;
}

function LiveRunCard({ onTerminal, taskUid }: Readonly<{ onTerminal: () => void; taskUid: string }>) {
  const [run, setRun] = useState<BrowserRunView | null>(null);
  useEffect(() => {
    const source = new EventSource(`/app/api/v1/runs/${encodeURIComponent(taskUid)}/events`);
    const update = (event: MessageEvent<string>) => {
      const snapshot = parseRunEventData(event.data);
      if (!snapshot) return;
      setRun(snapshot.run);
      if (snapshot.run.finalized && ["succeeded", "failed", "cancelled"].includes(snapshot.run.phase)) {
        source.close();
        onTerminal();
      }
    };
    source.addEventListener("snapshot", update as EventListener);
    source.addEventListener("run", update as EventListener);
    return () => source.close();
  }, [onTerminal, taskUid]);
  return <div className="rounded-tile border p-4" aria-live="polite">{run ? <RunSummary run={run} /> : <div className="flex items-center gap-3"><StatusBadge value="submitted" /><span className="font-mono text-xs">{taskUid}</span></div>}</div>;
}
