import {
  getBrowserPreferences,
  listProviderConnections,
  listPublishedWorkflows,
  type BrowserPreferencesView,
  type ConnectionsCollectionResponse,
  type EnvelopeRequestsResponse,
  type MyRunsResponse,
  type PublishedWorkflowsResponse,
} from "@/api-client";
import { loadAllEnvelopeRequests, loadAllMyRuns } from "@/data/paginated-api";

export type OnboardingEvidence = {
  connections: ConnectionsCollectionResponse;
  envelopes: EnvelopeRequestsResponse;
  preferences: BrowserPreferencesView;
  workflows: PublishedWorkflowsResponse;
  runs: MyRunsResponse;
};

export function matchingSampleRun(
  runs: MyRunsResponse["runs"],
  sampleWorkflow: string | null,
  provisionedEnvelopeIds: ReadonlySet<string>,
) {
  if (!sampleWorkflow) return undefined;
  return runs.find((run) => run.trigger?.provider === "github"
    && `${run.workflowName}@${run.workflowVersion}` === sampleWorkflow
    && Boolean(run.userEnvelopeInstanceId)
    && provisionedEnvelopeIds.has(String(run.userEnvelopeInstanceId)));
}

export function deriveOnboardingProgress(data: OnboardingEvidence) {
  const sample = data.workflows.workflows.find((workflow) => workflow.sample);
  const sampleWorkflow = sample ? `${sample.name}@${sample.version}` : null;
  const provisionedEnvelopeIds = new Set(data.envelopes.requests
    .filter((request) => request.status === "provisioned" && request.envelopeInstanceId)
    .map((request) => String(request.envelopeInstanceId)));
  const sampleRun = matchingSampleRun(data.runs.runs, sampleWorkflow, provisionedEnvelopeIds);
  const done = [
    data.connections.connections.some((connection) => connection.status.phase === "connected"),
    provisionedEnvelopeIds.size > 0,
    data.preferences.workflowAcknowledged,
    Boolean(sampleRun),
    Boolean(sampleRun && ["succeeded", "failed", "cancelled"].includes(sampleRun.phase)),
  ];
  return { completed: done.filter(Boolean).length, done, provisionedEnvelopeIds, sample, sampleRun, sampleWorkflow };
}

export async function loadOnboardingEvidence() {
  const [connections, envelopes, preferences, workflows, runs] = await Promise.all([
    listProviderConnections({ cache: "no-store", credentials: "same-origin" }),
    loadAllEnvelopeRequests("provisioned"),
    getBrowserPreferences({ cache: "no-store", credentials: "same-origin" }),
    listPublishedWorkflows({ cache: "no-store", credentials: "same-origin" }),
    loadAllMyRuns(),
  ]);
  const results = [connections, envelopes, preferences, workflows, runs];
  return {
    data: connections.data && envelopes.data && preferences.data && workflows.data && runs.data
      ? { connections: connections.data, envelopes: envelopes.data, preferences: preferences.data, workflows: workflows.data, runs: runs.data }
      : undefined,
    response: results.find((result) => !result.response?.ok)?.response ?? connections.response,
  };
}
