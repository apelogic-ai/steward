import {
  getBrowserPreferences,
  githubOnboardingEvidence,
  listProviderConnections,
  type BrowserPreferencesView,
  type ConnectionsCollectionResponse,
  type EnvelopeRequestsResponse,
  type GithubRepositoryView,
  type MyRunsResponse,
} from "@/api-client";
import { loadAllEnvelopeRequests, loadAllMyRuns } from "@/data/paginated-api";

export type OnboardingEvidence = {
  connections: ConnectionsCollectionResponse;
  envelopes: EnvelopeRequestsResponse;
  preferences: BrowserPreferencesView;
  runs: MyRunsResponse;
};

export type OnboardingAutomationEvidence = {
  dispatchObserved?: boolean;
  publicationObserved?: boolean;
  workflowObserved?: boolean;
};

export const ONBOARDING_PROGRESS_EVENT = "hypershell:onboarding-progress";

export function defaultOnboardingRepository(repositories: readonly GithubRepositoryView[]) {
  return repositories.find((repository) => repository.ready) ?? repositories[0];
}

export function browserHelloWorldRun(
  runs: MyRunsResponse["runs"],
  provisionedEnvelopeIds: ReadonlySet<string>,
) {
  return runs
    .filter((run) => run.origin === "browser"
      && run.package?.source === "inline"
      && run.phase === "succeeded"
      && run.finalized
      && Boolean(run.userEnvelopeInstanceId)
      && provisionedEnvelopeIds.has(String(run.userEnvelopeInstanceId)))
    .sort((left, right) => new Date(right.updatedAt).valueOf() - new Date(left.updatedAt).valueOf())[0];
}

export function automatedPackageRun(
  runs: MyRunsResponse["runs"],
  browserRun: MyRunsResponse["runs"][number] | undefined,
  provisionedEnvelopeIds: ReadonlySet<string>,
) {
  const digest = browserRun?.package?.contentDigest;
  if (!digest) return undefined;
  return runs
    .filter((run) => run.trigger?.provider === "github"
      && run.package?.contentDigest === digest
      && Boolean(run.userEnvelopeInstanceId)
      && provisionedEnvelopeIds.has(String(run.userEnvelopeInstanceId)))
    .sort((left, right) => new Date(right.updatedAt).valueOf() - new Date(left.updatedAt).valueOf())[0];
}

export function deriveOnboardingProgress(
  data: OnboardingEvidence,
  automationEvidence: OnboardingAutomationEvidence = {},
) {
  const provisionedEnvelopeIds = new Set(data.envelopes.requests
    .filter((request) => request.status === "provisioned" && request.envelopeInstanceId)
    .map((request) => String(request.envelopeInstanceId)));
  const helloWorldRun = browserHelloWorldRun(data.runs.runs, provisionedEnvelopeIds);
  const automationRun = automatedPackageRun(data.runs.runs, helloWorldRun, provisionedEnvelopeIds);
  const testSucceeded = helloWorldRun?.phase === "succeeded" && helloWorldRun.finalized;
  const publicationObserved = automationEvidence.publicationObserved || Boolean(automationRun);
  const workflowObserved = automationEvidence.workflowObserved || Boolean(automationRun);
  const dispatchObserved = automationEvidence.dispatchObserved || Boolean(automationRun);
  const automationTerminal = Boolean(automationRun?.finalized && ["succeeded", "failed", "cancelled"].includes(automationRun.phase));
  const done = [
    data.connections.connections.some((connection) => connection.status.phase === "connected"),
    provisionedEnvelopeIds.size > 0,
    testSucceeded,
    publicationObserved,
    workflowObserved,
    dispatchObserved,
    automationTerminal,
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

export async function loadOnboardingProgress() {
  const evidence = await loadOnboardingEvidence();
  if (!evidence.data || !evidence.response?.ok) {
    return { ...evidence, progress: undefined };
  }

  const baseProgress = deriveOnboardingProgress(evidence.data);
  const taskUid = baseProgress.helloWorldRun?.taskUid;
  if (!taskUid || evidence.data.preferences.onboardingDismissed || baseProgress.completed === 7) {
    return { ...evidence, progress: baseProgress };
  }

  const automation = await githubOnboardingEvidence({
    cache: "no-store",
    credentials: "same-origin",
    path: { task_uid: taskUid },
  });
  if (!automation.data || !automation.response?.ok) return { ...evidence, progress: baseProgress };
  return {
    ...evidence,
    progress: deriveOnboardingProgress(evidence.data, {
      dispatchObserved: automation.data.dispatchObserved,
      publicationObserved: automation.data.publicationObserved || automation.data.workflowObserved,
      workflowObserved: automation.data.workflowObserved,
    }),
  };
}
