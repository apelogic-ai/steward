"use client";

import Link from "next/link";
import { useCallback, useMemo, useState } from "react";

import {
  listTemplates,
  updateBrowserPreferences,
  type EnvelopeTemplatesResponse,
} from "@/api-client";
import { PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { type MutationFailureState } from "@/data/mutation-state";
import { deriveOnboardingProgress, loadOnboardingEvidence, matchingSampleRun, type OnboardingEvidence } from "@/data/onboarding-progress";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

type OnboardingData = OnboardingEvidence & {
  templates: EnvelopeTemplatesResponse;
};

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

export function sampleRunDone(
  runs: Array<{ trigger?: { provider?: string | null } | null; workflowName?: string | null; workflowVersion?: number | null; userEnvelopeInstanceId?: string | null }>,
  sampleWorkflow: string | null,
  provisionedEnvelopeIds: ReadonlySet<string>,
) {
  return Boolean(matchingSampleRun(runs as OnboardingEvidence["runs"]["runs"], sampleWorkflow, provisionedEnvelopeIds));
}

export function OnboardingView() {
  const load = useCallback(async () => {
    const [evidence, templates] = await Promise.all([
      loadOnboardingEvidence(),
      listTemplates({ cache: "no-store", credentials: "same-origin" }),
    ]);
    const response = !evidence.response?.ok ? evidence.response : templates.response;
    const data = evidence.data && templates.data
      ? { ...evidence.data, templates: templates.data }
      : undefined;
    return { data, response };
  }, []);
  const state = useApiResource<OnboardingData>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Five steps to your first governed agent run. An envelope is the budget, models and tools an agent may use; the workflow runs your agent inside it." title="Get started" />
      <ResourceBoundary state={state}>{(data) => <OnboardingChecklist data={data} />}</ResourceBoundary>
    </section>
  );
}

function OnboardingChecklist({ data }: Readonly<{ data: OnboardingData }>) {
  const session = useSession();
  const [workflowAcknowledgement, setWorkflowAcknowledgement] = useState<"idle" | "working" | "done" | MutationFailureState>("idle");
  const [dismissal, setDismissal] = useState<"idle" | "working" | "done" | "shown" | MutationFailureState>("idle");
  const [listening, setListening] = useState(false);
  const [selectedTemplateId, setSelectedTemplateId] = useState(data.templates.templates[0]?.id ?? "");
  const connectedConnection = data.connections.connections.find((connection) => connection.status.phase === "connected");
  const provisionedRequest = data.envelopes.requests.find((request) => request.status === "provisioned" && request.envelopeInstanceId);
  const progress = useMemo(() => deriveOnboardingProgress(data), [data]);
  const { sample, sampleRun, sampleWorkflow } = progress;
  const workflowReady = data.preferences.workflowAcknowledged || workflowAcknowledgement === "done";
  const done = [...progress.done];
  done[2] = workflowReady;
  const completed = done.filter(Boolean).length;
  const current = done.findIndex((value) => !value);
  const [openStep, setOpenStep] = useState(current === -1 ? 4 : current);

  async function updatePreferences(body: { onboardingDismissed?: boolean; workflowAcknowledged?: boolean }) {
    if (session.status !== "authenticated") return false;
    const result = await updateBrowserPreferences({ body, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf } });
    const accepted = Boolean(result.data && result.response?.ok);
    if (accepted && typeof body.onboardingDismissed === "boolean") {
      window.dispatchEvent(new CustomEvent("hypershell:preferences-updated", { detail: { onboardingDismissed: body.onboardingDismissed } }));
    }
    return accepted;
  }

  if ((data.preferences.onboardingDismissed && dismissal !== "shown") || dismissal === "done") {
    return <div className="rounded-card border bg-panel p-5"><p className="text-sm text-muted-ink">The onboarding guide is hidden. Your progress is preserved.</p><button className="mt-4 rounded-control border px-4 py-2 text-sm font-semibold" onClick={async () => { setDismissal("working"); setDismissal(await updatePreferences({ onboardingDismissed: false }) ? "shown" : "error"); }} type="button">Show guide again</button></div>;
  }

  const stepRows = [
    {
      title: "Connect GitHub",
      status: done[0] ? `Connected as ${connectedConnection?.status.accountEmail ?? "GitHub user"}` : "Authorize repository access",
      body: <div className="space-y-3"><p className="text-sm text-muted-ink">Connect the GitHub identity your governed agents will use.</p><div className="flex flex-wrap gap-3"><Link className="rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand" href="/connections">Connect GitHub</Link><Link className="self-center text-sm font-semibold" href="/connections">Manage connections</Link></div></div>,
    },
    {
      title: "Get your first envelope",
      status: done[1] ? `${provisionedRequest?.templateId ?? "Custom"} envelope ${provisionedRequest?.envelopeInstanceId} provisioned` : "Pick a template; within the ceiling it is provisioned instantly",
      body: <div className="space-y-4"><p className="text-sm text-muted-ink">Requests within a template&apos;s ceiling are approved and provisioned right away.</p><div className="grid grid-cols-[repeat(auto-fit,minmax(170px,1fr))] gap-3">{data.templates.templates.map((template) => <button aria-pressed={template.id === selectedTemplateId} className={`rounded-tile border p-4 text-left ${template.id === selectedTemplateId ? "border-brand bg-brand-soft" : "hover:bg-subtle"}`} key={template.id} onClick={() => setSelectedTemplateId(template.id)} type="button"><span className="flex justify-between gap-3"><strong className="text-sm">{template.displayName}</strong><span className="font-mono text-xs text-muted-ink">rev {template.revision}</span></span><span className="mt-2 block text-xs text-muted-ink">Up to {template.ceiling.spec.budget.monthlyLimit} {template.ceiling.spec.budget.currency}/mo · {template.ceiling.spec.ttl}</span></button>)}</div><div className="flex flex-wrap gap-3"><Link className="rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand" href={`/envelopes/new?template=${encodeURIComponent(selectedTemplateId)}`}>Request {data.templates.templates.find((template) => template.id === selectedTemplateId)?.displayName ?? "an"} envelope</Link><Link className="self-center text-sm font-semibold" href="/envelopes/new?type=custom">Customize limits instead</Link></div></div>,
    },
    {
      title: "Choose the sample package",
      status: done[2] ? sampleWorkflow ?? "Sample selected" : "Use the published sample now; a repository is optional",
      body: <div className="space-y-4">{!sample ? <p className="text-sm text-muted-ink">The deployment has no executable onboarding sample.</p> : <p className="text-sm text-muted-ink">Run the immutable published sample directly. The Run now page also lets you copy an inline package into a repository later.</p>}<div className="flex flex-wrap gap-3"><Link className="rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand" href={`/runs/new${sampleWorkflow ? `?workflow=${encodeURIComponent(sampleWorkflow)}` : ""}`}>Open Run now</Link><button className="rounded-control border px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={!sampleWorkflow || workflowAcknowledgement === "working"} onClick={async () => { setWorkflowAcknowledgement("working"); setWorkflowAcknowledgement(await updatePreferences({ workflowAcknowledged: true }) ? "done" : "error"); }} type="button">Use this sample</button></div></div>,
    },
    {
      title: "Trigger a test run",
      status: done[3] ? `Run ${sampleRun?.taskUid} detected` : "Start the sample from Steward",
      body: <div className="space-y-4"><p className="text-sm text-muted-ink">Choose the provisioned Envelope, click Run now, and Steward resolves and records the immutable package pin before execution.</p><div className="flex flex-wrap gap-3"><Link className="rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand" href={`/runs/new${sampleWorkflow ? `?workflow=${encodeURIComponent(sampleWorkflow)}` : ""}`}>Run sample</Link><button className="rounded-control border px-4 py-2 text-sm font-semibold" onClick={() => setListening(true)} type="button">I&apos;ve started it</button></div>{listening && !sampleRun ? <p className="text-sm text-info" role="status">● Listening for a run from this envelope…</p> : null}</div>,
    },
    {
      title: "See the result",
      status: done[4] && sampleRun ? `${sampleRun.phase.charAt(0).toUpperCase()}${sampleRun.phase.slice(1)} in ${runDuration(sampleRun.createdAt, sampleRun.updatedAt)} · ${sampleRun.observedSpend?.observedAmount ?? "—"} ${sampleRun.observedSpend?.currency ?? ""}` : "Watch the run and open its log",
      body: sampleRun ? <div className="space-y-4 rounded-tile border p-4"><div className="flex flex-wrap items-start justify-between gap-4"><div><div className="flex items-center gap-2"><StatusBadge value={sampleRun.phase} /><strong>{sampleRun.workflow}</strong></div><p className="mt-1 font-mono text-xs text-muted-ink">{sampleRun.taskUid}</p></div><p className="text-sm text-muted-ink">{sampleRun.observedSpend ? `${sampleRun.observedSpend.observedAmount} ${sampleRun.observedSpend.currency}` : "Spend not reported"}</p></div><div className="rounded-control bg-code p-3 font-mono text-xs leading-5 text-code-ink">admission complete<br />runtime provisioned<br />agent execution {sampleRun.phase}</div><div className="flex flex-wrap gap-3"><Link className="rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand" href={`/runs/${sampleRun.taskUid}`}>Open run</Link><Link className="rounded-control border px-4 py-2 text-sm font-semibold" href="/envelopes">Go to envelopes</Link></div></div> : <p className="text-sm text-muted-ink">The run appears here as soon as it starts.</p>,
    },
  ];

  return <div className="overflow-hidden rounded-card border bg-panel">
    <div className="flex items-center gap-4 px-5 py-4"><strong className="whitespace-nowrap text-sm">{completed} of 5 done</strong><div aria-label={`${completed} of 5 onboarding steps complete`} className="h-1.5 flex-1 overflow-hidden rounded-full bg-line-soft" role="progressbar"><div className="h-full rounded-full bg-brand transition-[width] duration-300" style={{ width: `${completed * 20}%` }} /></div></div>
    <ol>{stepRows.map((step, index) => <li className="border-t border-line-soft" key={step.title}><button aria-expanded={openStep === index} className="grid w-full grid-cols-[28px_minmax(0,1fr)_auto_14px] items-center gap-3.5 px-5 py-4 text-left hover:bg-subtle" onClick={() => setOpenStep(openStep === index ? -1 : index)} type="button"><span aria-hidden="true" className={`grid size-7 place-items-center rounded-full text-[13px] font-bold ${done[index] ? "bg-ok text-panel" : index === current ? "bg-brand text-on-brand" : "shadow-[inset_0_0_0_1.5px_var(--color-field)] text-muted-ink"}`}>{done[index] ? "✓" : index + 1}</span><span><strong className={`block text-[15px] ${done[index] ? "text-muted-ink" : ""}`}>{step.title}</strong><span className="mt-0.5 block text-[13px] text-muted-ink">{step.status}</span></span>{done[index] ? <span className="rounded-full bg-ok-soft px-2.5 py-1 text-xs font-semibold text-ok">Done</span> : <span />}<span aria-hidden="true" className={`transition-transform ${openStep === index ? "rotate-90" : ""}`}>›</span></button>{openStep === index ? <div className="space-y-3 pb-5 pl-[62px] pr-5">{step.body}</div> : null}</li>)}</ol>
    <div className="border-t border-line-soft px-5 py-4"><button className="text-sm text-muted-ink hover:text-ink" disabled={dismissal === "working"} onClick={async () => { setDismissal("working"); setDismissal(await updatePreferences({ onboardingDismissed: true }) ? "done" : "error"); }} type="button">Hide this guide · you can reopen it from Settings.</button></div>
  </div>;
}
