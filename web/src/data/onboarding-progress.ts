import {
  detectWorkflow,
  getBrowserPreferences,
  githubAutomationEvidence,
  listProviderConnections,
  listRepositories,
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

export async function loadOnboardingProgress(csrf: string) {
  const evidence = await loadOnboardingEvidence();
  if (!evidence.data || !evidence.response?.ok) {
    return { ...evidence, progress: undefined };
  }

  const baseProgress = deriveOnboardingProgress(evidence.data);
  const taskUid = baseProgress.helloWorldRun?.taskUid;
  if (!taskUid) return { ...evidence, progress: baseProgress };

  const repositories = await listRepositories({
    cache: "no-store",
    credentials: "same-origin",
    query: { query: "", page: 1, perPage: 100 },
  });
  const repository = defaultOnboardingRepository(repositories.data?.repositories ?? []);
  if (!repository?.ready) return { ...evidence, progress: baseProgress };

  const [automation, workflow] = await Promise.all([
    githubAutomationEvidence({
      cache: "no-store",
      credentials: "same-origin",
      path: { task_uid: taskUid },
      query: { owner: repository.owner, repository: repository.name },
    }),
    detectWorkflow({
      body: { owner: repository.owner, repository: repository.name },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": csrf },
      path: { task_uid: taskUid },
    }),
  ]);
  const workflowObserved = Boolean(workflow.data && workflow.response?.ok && workflow.data.compatible);
  return {
    ...evidence,
    progress: deriveOnboardingProgress(evidence.data, {
      dispatchObserved: Boolean(automation.data && automation.response?.ok && automation.data.dispatch),
      publicationObserved: Boolean(automation.data && automation.response?.ok && automation.data.publication) || workflowObserved,
      workflowObserved,
    }),
  };
}
