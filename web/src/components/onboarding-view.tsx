"use client";

import { useCallback, useState, useSyncExternalStore } from "react";

import {
  getBrowserPreferences,
  listProviderConnections,
  listRequests,
  myRuns,
  updateBrowserPreferences,
  type BrowserPreferencesView,
  type ConnectionsCollectionResponse,
  type EnvelopeRequestsResponse,
  type MyRunsResponse,
} from "@/api-client";
import { PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { classifyMutationFailure, type MutationFailureState } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";
import { ONBOARDING_WORKFLOW_PATH_KEY } from "@/workflows/contracts";

type OnboardingData = {
  connections: ConnectionsCollectionResponse;
  envelopes: EnvelopeRequestsResponse;
  preferences: BrowserPreferencesView;
  runs: MyRunsResponse;
};

export function workflowSetupDone(
  runs: Array<{ trigger?: null | { callerWorkflow: string } }>,
  acknowledged: boolean,
  suggestedPath: string | null,
) {
  if (acknowledged) return true;
  if (!suggestedPath) return false;
  return runs.some((run) => {
    const workflowRef = run.trigger?.callerWorkflow.split("@", 1)[0];
    const pathStart = workflowRef?.indexOf("/.github/workflows/") ?? -1;
    return pathStart >= 0 && workflowRef?.slice(pathStart + 1) === suggestedPath;
  });
}

export function OnboardingView() {
  const session = useSession();
  const [workflowAcknowledged, setWorkflowAcknowledged] = useState(false);
  const suggestedWorkflowPath = useSyncExternalStore(
    () => () => undefined,
    () => localStorage.getItem(ONBOARDING_WORKFLOW_PATH_KEY),
    () => null,
  );
  const [dismissal, setDismissal] = useState<"idle" | "working" | "done" | MutationFailureState>("idle");
  const load = useCallback(async () => {
    const [connections, envelopes, preferences, runs] = await Promise.all([
      listProviderConnections({ cache: "no-store", credentials: "same-origin" }),
      listRequests({ cache: "no-store", credentials: "same-origin", query: {} }),
      getBrowserPreferences({ cache: "no-store", credentials: "same-origin" }),
      myRuns({ cache: "no-store", credentials: "same-origin", query: { limit: 100 } }),
    ]);
    const response = [connections, envelopes, preferences, runs].find((result) => !result.response?.ok)?.response
      ?? connections.response;
    const data = connections.data && envelopes.data && preferences.data && runs.data ? {
      connections: connections.data,
      envelopes: envelopes.data,
      preferences: preferences.data,
      runs: runs.data,
    } : undefined;
    return { data, response };
  }, []);
  const state = useApiResource<OnboardingData>(load);

  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Set up a governed envelope and run the first published Workflow." title="Get started" />
      <ResourceBoundary state={state}>{(data) => {
        const connected = data.connections.connections.some((connection) => connection.status.phase === "connected");
        const provisioned = data.envelopes.requests.some((request) => request.status === "provisioned");
        const workflowReady = workflowSetupDone(data.runs.runs, workflowAcknowledged, suggestedWorkflowPath);
        const firstRun = data.runs.runs.length > 0;
        const steps = [
          ["Connect GitHub", connected, "Authorize GitHub from Connections."],
          ["Provision an envelope", provisioned, "Choose any eligible named envelope template."],
          ["Add the generated workflow", workflowReady, "Render the sample Workflow from the envelope detail and commit it to GitHub."],
          ["Run the test workflow", firstRun, "Run it from GitHub with gh workflow run or the Actions UI."],
          ["Inspect the governed run", firstRun, "Return here to inspect stages, logs, provenance, and spend."],
        ] as const;
        if (data.preferences.onboardingDismissed || dismissal === "done") {
          return <p className="rounded-panel border bg-panel p-6 text-sm text-muted-ink">Onboarding is dismissed for your account. You can still use the links in the main navigation.</p>;
        }
        return (
          <div className="space-y-5">
            <ol className="space-y-3">{steps.map(([label, done, detail], index) => <li className="rounded-panel border bg-panel p-5 shadow-sm" key={label}><div className="flex items-center justify-between gap-3"><h2 className="font-semibold">{index + 1}. {label}</h2><StatusBadge value={done ? "done" : "pending"} /></div><p className="mt-2 text-sm text-muted-ink">{detail}</p></li>)}</ol>
            {!workflowReady ? <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold" onClick={() => setWorkflowAcknowledged(true)} type="button">I added the workflow</button> : null}
            <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={dismissal === "working"} onClick={async () => {
              if (session.status !== "authenticated") return;
              setDismissal("working");
              const result = await updateBrowserPreferences({ body: { onboardingDismissed: true }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf } });
              setDismissal(result.data && result.response?.ok ? "done" : classifyMutationFailure(result.response?.status));
            }} type="button">Dismiss checklist</button>
            {dismissal !== "idle" && dismissal !== "working" ? <p className="text-sm text-red-800" role="alert">The checklist preference could not be saved ({dismissal}).</p> : null}
          </div>
        );
      }}</ResourceBoundary>
    </section>
  );
}
