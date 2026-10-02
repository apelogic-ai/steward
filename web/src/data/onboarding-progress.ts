import {
  getBrowserPreferences,
  listProviderConnections,
  type BrowserPreferencesView,
  type ConnectionsCollectionResponse,
  type EnvelopeRequestsResponse,
  type MyRunsResponse,
} from "@/api-client";
import { loadAllEnvelopeRequests, loadAllMyRuns } from "@/data/paginated-api";

export type OnboardingEvidence = {
  connections: ConnectionsCollectionResponse;
  envelopes: EnvelopeRequestsResponse;
  preferences: BrowserPreferencesView;
  runs: MyRunsResponse;
};

export function browserHelloWorldRun(
  runs: MyRunsResponse["runs"],
  provisionedEnvelopeIds: ReadonlySet<string>,
) {
  return runs.find((run) => run.origin === "browser"
    && run.package?.source === "inline"
    && Boolean(run.userEnvelopeInstanceId)
    && provisionedEnvelopeIds.has(String(run.userEnvelopeInstanceId)));
}

export function automatedPackageRun(
  runs: MyRunsResponse["runs"],
  browserRun: MyRunsResponse["runs"][number] | undefined,
  provisionedEnvelopeIds: ReadonlySet<string>,
) {
  const digest = browserRun?.package?.contentDigest;
  if (!digest) return undefined;
  return runs.find((run) => run.trigger?.provider === "github"
    && run.package?.contentDigest === digest
    && Boolean(run.userEnvelopeInstanceId)
    && provisionedEnvelopeIds.has(String(run.userEnvelopeInstanceId)));
}

export function deriveOnboardingProgress(data: OnboardingEvidence) {
  const provisionedEnvelopeIds = new Set(data.envelopes.requests
    .filter((request) => request.status === "provisioned" && request.envelopeInstanceId)
    .map((request) => String(request.envelopeInstanceId)));
  const helloWorldRun = browserHelloWorldRun(data.runs.runs, provisionedEnvelopeIds);
  const automationRun = automatedPackageRun(data.runs.runs, helloWorldRun, provisionedEnvelopeIds);
  const done = [
    data.connections.connections.some((connection) => connection.status.phase === "connected"),
    provisionedEnvelopeIds.size > 0,
    Boolean(helloWorldRun),
    helloWorldRun?.phase === "succeeded",
  ];
  return { completed: done.filter(Boolean).length, done, provisionedEnvelopeIds, helloWorldRun, automationRun };
}

export async function loadOnboardingEvidence() {
  const [connections, envelopes, preferences, runs] = await Promise.all([
    listProviderConnections({ cache: "no-store", credentials: "same-origin" }),
    loadAllEnvelopeRequests("provisioned"),
    getBrowserPreferences({ cache: "no-store", credentials: "same-origin" }),
    loadAllMyRuns(),
  ]);
  const results = [connections, envelopes, preferences, runs];
  return {
    data: connections.data && envelopes.data && preferences.data && runs.data
      ? { connections: connections.data, envelopes: envelopes.data, preferences: preferences.data, runs: runs.data }
      : undefined,
    response: results.find((result) => !result.response?.ok)?.response ?? connections.response,
  };
}
