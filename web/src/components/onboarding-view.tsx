"use client";

import Link from "next/link";
import { useCallback, useState } from "react";

import {
  getBrowserPreferences,
  listProviderConnections,
  listPublishedWorkflows,
  listRequests,
  myRuns,
  updateBrowserPreferences,
  type BrowserPreferencesView,
  type ConnectionsCollectionResponse,
  type EnvelopeRequestsResponse,
  type MyRunsResponse,
  type PublishedWorkflowsResponse,
} from "@/api-client";
import { SectionCard } from "@/components/hs";
import { PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { classifyMutationFailure, type MutationFailureState } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

type OnboardingData = {
  connections: ConnectionsCollectionResponse;
  envelopes: EnvelopeRequestsResponse;
  preferences: BrowserPreferencesView;
  workflows: PublishedWorkflowsResponse;
  runs: MyRunsResponse;
};

async function loadAllProvisionedEnvelopes() {
  const requests: EnvelopeRequestsResponse["requests"] = [];
  const seen = new Set<string>();
  let cursor: string | undefined;
  for (;;) {
    const page = await listRequests({
      cache: "no-store",
      credentials: "same-origin",
      query: { cursor, limit: 100, status: "provisioned" },
    });
    if (!page.data || !page.response?.ok) return page;
    requests.push(...page.data.requests);
    const nextCursor = page.data.nextCursor ?? undefined;
    if (!nextCursor) return {
      data: { ...page.data, nextCursor: null, requests },
      response: page.response,
    };
    if (seen.has(nextCursor)) return {
      data: undefined,
      response: new Response(null, { status: 502 }),
    };
    seen.add(nextCursor);
    cursor = nextCursor;
  }
}

async function loadAllRuns() {
  const runs: MyRunsResponse["runs"] = [];
  const seen = new Set<string>();
  let cursor: string | undefined;
  for (;;) {
    const page = await myRuns({
      cache: "no-store",
      credentials: "same-origin",
      query: { cursor, limit: 100 },
    });
    if (!page.data || !page.response?.ok) return page;
    runs.push(...page.data.runs);
    const nextCursor = page.data.nextCursor ?? undefined;
    if (!nextCursor) return {
      data: { ...page.data, nextCursor: null, runs },
      response: page.response,
    };
    if (seen.has(nextCursor)) return {
      data: undefined,
      response: new Response(null, { status: 502 }),
    };
    seen.add(nextCursor);
    cursor = nextCursor;
  }
}

export function sampleRunDone(
  runs: Array<{
    trigger?: { provider?: string | null } | null;
    workflowName?: string | null;
    workflowVersion?: number | null;
    userEnvelopeInstanceId?: string | null;
  }>,
  sampleWorkflow: string | null,
  provisionedEnvelopeIds: ReadonlySet<string>,
) {
  if (!sampleWorkflow) return false;
  return runs.some((run) => run.trigger?.provider === "github"
    && `${run.workflowName}@${run.workflowVersion}` === sampleWorkflow
    && Boolean(run.userEnvelopeInstanceId)
    && provisionedEnvelopeIds.has(String(run.userEnvelopeInstanceId)));
}

export function OnboardingView() {
  const session = useSession();
  const [workflowAcknowledgement, setWorkflowAcknowledgement] = useState<"idle" | "working" | "done" | MutationFailureState>("idle");
  const [dismissal, setDismissal] = useState<"idle" | "working" | "done" | "shown" | MutationFailureState>("idle");
  const load = useCallback(async () => {
    const [connections, envelopes, preferences, workflows, runs] = await Promise.all([
      listProviderConnections({ cache: "no-store", credentials: "same-origin" }),
      loadAllProvisionedEnvelopes(),
      getBrowserPreferences({ cache: "no-store", credentials: "same-origin" }),
      listPublishedWorkflows({ cache: "no-store", credentials: "same-origin" }),
      loadAllRuns(),
    ]);
    const response = [connections, envelopes, preferences, workflows, runs].find((result) => !result.response?.ok)?.response
      ?? connections.response;
    const data = connections.data && envelopes.data && preferences.data && workflows.data && runs.data ? {
      connections: connections.data,
      envelopes: envelopes.data,
      preferences: preferences.data,
      workflows: workflows.data,
      runs: runs.data,
    } : undefined;
    return { data, response };
  }, []);
  const state = useApiResource<OnboardingData>(load);

  async function setOnboardingDismissed(value: boolean) {
    if (session.status !== "authenticated") return;
    setDismissal("working");
    const result = await updateBrowserPreferences({ body: { onboardingDismissed: value }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf } });
    if (result.data && result.response?.ok) {
      setDismissal(value ? "done" : "shown");
      window.dispatchEvent(new CustomEvent("hypershell:preferences-updated", { detail: { onboardingDismissed: value } }));
    } else setDismissal(classifyMutationFailure(result.response?.status));
  }

  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Set up a governed envelope and run Steward's sample Workflow." title="Get started" />
      <ResourceBoundary state={state}>{(data) => {
        const connected = data.connections.connections.some((connection) => connection.status.phase === "connected");
        const provisionedEnvelopeIds = new Set(data.envelopes.requests
          .filter((request) => request.status === "provisioned" && request.envelopeInstanceId)
          .map((request) => String(request.envelopeInstanceId)));
        const provisioned = provisionedEnvelopeIds.size > 0;
        const sample = data.workflows.workflows.find((workflow) => workflow.sample);
        const sampleWorkflow = sample ? `${sample.name}@${sample.version}` : null;
        const workflowReady = data.preferences.workflowAcknowledged || workflowAcknowledgement === "done";
        const firstRun = sampleRunDone(data.runs.runs, sampleWorkflow, provisionedEnvelopeIds);
        const steps = [
          ["Connect GitHub", connected, "Authorize GitHub from Connections.", "/connections", "Open connections"],
          ["Provision an envelope", provisioned, "Choose any eligible named envelope template.", "/envelopes/new", "Request envelope"],
          ["Add the generated workflow", workflowReady, sampleWorkflow
            ? `Render ${sampleWorkflow} from the envelope detail and commit its GitHub Actions file.`
            : "The deployment has no executable onboarding sample.", "/envelopes", "Open envelopes"],
          ["Run the test workflow", firstRun, "Run it from GitHub with gh workflow run or the Actions UI.", "/runs", "Check runs"],
          ["Inspect the governed run", firstRun, "Return here to inspect stages, logs, provenance, and spend.", "/runs", "Open runs"],
        ] as const;
        if ((data.preferences.onboardingDismissed && dismissal !== "shown") || dismissal === "done") {
          return <SectionCard title="Checklist dismissed"><p className="text-sm text-muted-ink">The onboarding link is hidden from your sidebar. Your progress is still preserved.</p><button className="mt-5 min-h-10 rounded-control border px-4 py-2 text-sm font-semibold" disabled={dismissal === "working"} onClick={() => void setOnboardingDismissed(false)} type="button">Show checklist again</button></SectionCard>;
        }
        const completed = steps.filter(([, done]) => done).length;
        return (
          <div className="grid items-start gap-6 lg:grid-cols-[minmax(0,1fr)_300px]">
            <SectionCard title="Setup checklist">
              <ol className="divide-y divide-line-soft">{steps.map(([label, done, detail, href, action], index) => <li className="grid gap-3 py-5 first:pt-0 last:pb-0 sm:grid-cols-[36px_minmax(0,1fr)_auto] sm:items-start" key={label}><span aria-hidden="true" className={`grid size-8 place-items-center rounded-full text-sm font-bold ${done ? "bg-ok-soft text-ok" : "border bg-panel text-muted-ink"}`}>{done ? "✓" : index + 1}</span><div><h2 className="font-semibold">{index + 1}. {label}</h2><p className="mt-1 text-sm text-muted-ink">{detail}</p></div><div className="flex items-center gap-3"><StatusBadge value={done ? "done" : "pending"} />{!done ? <Link className="text-sm font-semibold" href={href}>{action}</Link> : null}</div></li>)}</ol>
            </SectionCard>
            <aside className="space-y-5">
              <SectionCard title="Progress"><p className="text-4xl font-semibold tracking-tight">{completed}<span className="text-xl text-muted-ink"> / {steps.length}</span></p><div aria-label={`${completed} of ${steps.length} onboarding steps complete`} className="mt-4 h-2 overflow-hidden rounded-full bg-line-soft"><div className={`h-full rounded-full bg-brand ${completed === 0 ? "w-0" : completed === 1 ? "w-1/5" : completed === 2 ? "w-2/5" : completed === 3 ? "w-3/5" : completed === 4 ? "w-4/5" : "w-full"}`} /></div><p className="mt-3 text-sm text-muted-ink">Complete the steps in order to prove a governed GitHub workflow end to end.</p></SectionCard>
            {sampleWorkflow && !workflowReady ? <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={workflowAcknowledgement === "working"} onClick={async () => {
              if (session.status !== "authenticated") return;
              setWorkflowAcknowledgement("working");
              const result = await updateBrowserPreferences({ body: { workflowAcknowledged: true }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf } });
              setWorkflowAcknowledgement(result.data && result.response?.ok ? "done" : classifyMutationFailure(result.response?.status));
            }} type="button">I added the sample workflow</button> : null}
            {workflowAcknowledgement !== "idle" && workflowAcknowledgement !== "working" && workflowAcknowledgement !== "done" ? <p className="text-sm text-red-800" role="alert">The workflow acknowledgement could not be saved ({workflowAcknowledgement}).</p> : null}
              <button className="min-h-10 w-full rounded-control border px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={dismissal === "working"} onClick={() => void setOnboardingDismissed(true)} type="button">Dismiss checklist</button>
              {dismissal !== "idle" && dismissal !== "working" && dismissal !== "shown" ? <p className="text-sm text-red-800" role="alert">The checklist preference could not be saved ({dismissal}).</p> : null}
            </aside>
          </div>
        );
      }}</ResourceBoundary>
    </section>
  );
}
