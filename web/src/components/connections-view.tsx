"use client";

import { useCallback, useEffect, useRef, useState } from "react";

import {
  disconnectProviderConnection,
  getInferenceConnection,
  getProviderConnectionStartOperation,
  listProviderConnections,
  removeInferenceCredential,
  saveInferenceCredential,
  startProviderConnection,
  type ConnectionOperationErrorResponse,
  type ConnectionsCollectionResponse,
  type InferenceConnectionResponse,
  type ProviderConnectionView,
} from "@/api-client";
import { connectionHealth } from "@/components/connection-health";
import { classifyConnectionMutationFailure, type ConnectionMutationState } from "@/components/connection-mutation-state";
import { boundedConnectionPollDeadline } from "@/components/connection-poll-deadline";
import { ConfirmationDialog, SectionCard } from "@/components/hs";
import { PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { useInvalidateGithubRepositories } from "@/data/github-repositories";
import { useApiResource, type ResourceState } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

export function ConnectionsView() {
  const [generation, setGeneration] = useState(0);
  const load = useCallback(() => {
    void generation;
    return listProviderConnections({ cache: "no-store", credentials: "same-origin" });
  }, [generation]);
  const state = useApiResource<ConnectionsCollectionResponse>(load);
  const loadInference = useCallback(() => {
    void generation;
    return getInferenceConnection({ cache: "no-store", credentials: "same-origin" });
  }, [generation]);
  const inferenceState = useApiResource<InferenceConnectionResponse>(loadInference);
  if (state.status === "forbidden" || state.status === "not-found") {
    return <section aria-labelledby="page-title" className="space-y-6"><PageHeader description="Accounts your agents act through. HyperShell holds the credentials; agents never see them." title="Connections" /><ResourceBoundary state={state}>{() => null}</ResourceBoundary></section>;
  }
  const canAdd = state.status === "ready" && state.value.available.some((candidate) => candidate.enabled
    && !state.value.connections.some((connection) => connection.provider === candidate.provider));
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Accounts your agents act through. HyperShell holds the credentials; agents never see them." title="Connections" />
      <div className="space-y-3">
        <div className="flex items-center justify-between gap-4"><div><h2 className="text-lg font-semibold">Tools / MCP servers</h2><p className="text-sm text-muted-ink">Authorize tool providers used by governed agents.</p></div><button className="h-10 rounded-control bg-brand px-4 text-sm font-semibold text-on-brand disabled:cursor-not-allowed disabled:opacity-40" disabled={!canAdd} type="button">+ Add connection</button></div>
        {state.status === "ready" ? state.value.connections.map((connection) => <ProviderConnection connection={connection} key={connection.provider} refresh={() => setGeneration((value) => value + 1)} />) : <ProviderConnection metadataState={state.status} refresh={() => setGeneration((value) => value + 1)} />}
      </div>
      <div className="space-y-3">
        <div><h2 className="text-lg font-semibold">Inference / LLMs</h2><p className="text-sm text-muted-ink">Configure how governed agents authenticate to the inference gateway.</p></div>
        <InferenceConnectionCard state={inferenceState} refresh={() => setGeneration((value) => value + 1)} />
      </div>
    </section>
  );
}

function InferenceConnectionCard({ state, refresh }: Readonly<{
  state: ResourceState<InferenceConnectionResponse>;
  refresh: () => void;
}>) {
  const session = useSession();
  const [editing, setEditing] = useState(false);
  const [removing, setRemoving] = useState(false);
  const [key, setKey] = useState("");
  const [action, setAction] = useState<"idle" | "working" | "error">("idle");
  const value = state.status === "ready" ? state.value : undefined;

  async function save() {
    if (session.status !== "authenticated" || key.length < 4) return;
    setAction("working");
    try {
      const result = await saveInferenceCredential({
        body: { apiKey: key },
        cache: "no-store",
        credentials: "same-origin",
        headers: { "X-Steward-CSRF": session.value.csrf },
      });
      if (result.response?.status !== 200) {
        setAction("error");
        return;
      }
      setKey("");
      setEditing(false);
      setAction("idle");
      refresh();
    } catch {
      setAction("error");
    }
  }

  async function remove() {
    if (session.status !== "authenticated") return;
    setAction("working");
    try {
      const result = await removeInferenceCredential({
        cache: "no-store",
        credentials: "same-origin",
        headers: { "X-Steward-CSRF": session.value.csrf },
      });
      if (result.response?.status !== 204) {
        setAction("error");
        return;
      }
      setRemoving(false);
      setAction("idle");
      refresh();
    } catch {
      setAction("error");
    }
  }

  if (state.status !== "ready") {
    return <SectionCard actions={<StatusBadge value={state.status === "loading" ? "Checking" : "Unavailable"} />} title="Inference gateway"><p className="text-sm text-muted-ink">Inference connection metadata is not currently available.</p></SectionCard>;
  }
  if (value?.mode === "stock") {
    return <SectionCard actions={<StatusBadge value="Administrator managed" />} title="Inference gateway"><p className="text-sm text-muted-ink">Inference credentials are managed by your Steward administrator. No personal API key is required.</p></SectionCard>;
  }
  const credential = value?.credential;
  return (
    <SectionCard actions={<StatusBadge value={credential ? "Configured" : "Key required"} />} title="Inference gateway">
      <div className="space-y-4">
        {credential ? <p className="text-sm">Saved {new Date(credential.savedAt).toLocaleString()} · key ending in <span className="font-mono">{credential.lastFour}</span></p> : <p className="text-sm text-muted-ink">Add your inference gateway API key before starting a task that uses a model.</p>}
        {editing ? <div className="space-y-3"><label className="block text-sm font-semibold" htmlFor="inference-api-key">Inference API key</label><input autoComplete="off" className="h-10 w-full max-w-xl rounded-control border bg-panel px-3 font-mono text-sm" id="inference-api-key" onChange={(event) => setKey(event.target.value)} type="password" value={key} /><div className="flex gap-2"><button className="h-10 rounded-control bg-brand px-4 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={action === "working" || key.length < 4} onClick={() => void save()} type="button">Save key</button><button className="h-10 rounded-control border px-4 text-sm font-semibold" disabled={action === "working"} onClick={() => { setKey(""); setEditing(false); setAction("idle"); }} type="button">Cancel</button></div></div> : <div className="flex gap-2"><button className="h-10 rounded-control bg-brand px-4 text-sm font-semibold text-on-brand" onClick={() => setEditing(true)} type="button">{credential ? "Replace key" : "Add key"}</button>{credential ? <button className="h-10 rounded-control border border-err px-4 text-sm font-semibold text-err" onClick={() => setRemoving(true)} type="button">Remove key…</button> : null}</div>}
        {action === "error" ? <p className="text-sm text-err" role="alert">The inference key could not be saved. Check the key and retry.</p> : null}
        <ConfirmationDialog cancelLabel="Keep key" confirmLabel="Remove key" description="This removes Steward's encrypted copy and blocks new model tasks. It does not revoke or delete the key at the upstream inference service." onConfirm={() => void remove()} onOpenChange={setRemoving} open={removing} pending={action === "working"} title="Remove inference key?" />
      </div>
    </SectionCard>
  );
}

function ProviderConnection({ connection, metadataState = "ready", refresh }: Readonly<{
  connection?: ProviderConnectionView;
  metadataState?: "loading" | "ready" | "unavailable" | "error";
  refresh: () => void;
}>) {
  const session = useSession();
  const invalidateRepositories = useInvalidateGithubRepositories();
  const startController = useRef<AbortController | null>(null);
  const [disconnectOpen, setDisconnectOpen] = useState(false);
  const [action, setAction] = useState<"idle" | "working" | "poll-expired" | ConnectionMutationState>("idle");
  const [startFailure, setStartFailure] = useState<ConnectionOperationErrorResponse | null>(null);
  const status = connection?.status;
  const health = status ? connectionHealth(status) : undefined;
  const reauthorizationRecommended = health === "expiring_soon" || health === "expired";
  const badge = metadataState === "loading" ? "Checking" : !status ? "Status unavailable" : health === "expiring_soon" ? "Expiring soon" : health === "expired" ? "Credential expired" : status.phase;

  useEffect(() => () => startController.current?.abort(), []);

  async function connect() {
    if (session.status !== "authenticated") return;
    startController.current?.abort();
    const controller = new AbortController();
    startController.current = controller;
    setAction("working");
    setStartFailure(null);
    const provider = connection?.provider ?? "github";
    let deadlineTimer: ReturnType<typeof setTimeout> | undefined;
    try {
      const result = await startProviderConnection({ body: {}, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { provider }, signal: controller.signal });
      if (result.response?.status !== 202 || !result.data?.operationId || !result.data.pollDeadlineAt) {
        setStartFailure(connectionOperationError(result.error));
        setAction(classifyConnectionMutationFailure(result.response?.status, result.error));
        return;
      }
      const pollDeadline = boundedConnectionPollDeadline(result.data.pollDeadlineAt);
      if (pollDeadline === null) {
        setAction("error");
        return;
      }
      const expirePoll = () => {
        if (startController.current !== controller || controller.signal.aborted) return;
        setAction("poll-expired");
        controller.abort();
      };
      if (pollDeadline <= Date.now()) {
        expirePoll();
        return;
      }
      deadlineTimer = setTimeout(expirePoll, pollDeadline - Date.now());
      while (!controller.signal.aborted) {
        const operation = await getProviderConnectionStartOperation({ cache: "no-store", credentials: "same-origin", path: { provider, operation_id: result.data.operationId }, signal: controller.signal });
        if (controller.signal.aborted) return;
        if (operation.data?.state === "succeeded" && operation.data.authorizationUrl) {
          window.location.assign(operation.data.authorizationUrl);
          return;
        }
        if (operation.data?.state === "failed") {
          setStartFailure(operation.data.error ? {
            apiVersion: operation.data.apiVersion,
            error: operation.data.error,
            code: operation.data.code,
            upstreamStatus: operation.data.upstreamStatus,
            detail: operation.data.detail,
          } : null);
          setAction("error");
          return;
        }
        if (operation.response?.status !== 202 || operation.data?.state !== "pending") {
          setStartFailure(connectionOperationError(operation.error));
          setAction(classifyConnectionMutationFailure(operation.response?.status, operation.error));
          return;
        }
        if (!await waitForConnectionPoll(controller.signal)) return;
      }
    } catch {
      if (!controller.signal.aborted) setAction("error");
    } finally {
      if (deadlineTimer !== undefined) clearTimeout(deadlineTimer);
      if (startController.current === controller) startController.current = null;
    }
  }

  async function disconnect() {
    if (session.status !== "authenticated") return;
    startController.current?.abort();
    const controller = new AbortController();
    startController.current = controller;
    setStartFailure(null);
    setAction("working");
    const provider = connection?.provider ?? "github";
    let deadlineTimer: ReturnType<typeof setTimeout> | undefined;
    try {
      const result = await disconnectProviderConnection({ body: { confirm: true }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { provider }, signal: controller.signal });
      if (result.response?.status !== 202 || !result.data?.operationId || !result.data.pollDeadlineAt) {
        setStartFailure(connectionOperationError(result.error));
        setAction(classifyConnectionMutationFailure(result.response?.status, result.error));
        return;
      }
      const pollDeadline = boundedConnectionPollDeadline(result.data.pollDeadlineAt);
      if (pollDeadline === null) { setAction("error"); return; }
      const expirePoll = () => {
        if (startController.current !== controller || controller.signal.aborted) return;
        setAction("poll-expired");
        controller.abort();
      };
      if (pollDeadline <= Date.now()) { expirePoll(); return; }
      deadlineTimer = setTimeout(expirePoll, pollDeadline - Date.now());
      while (!controller.signal.aborted) {
        const operation = await getProviderConnectionStartOperation({ cache: "no-store", credentials: "same-origin", path: { provider, operation_id: result.data.operationId }, signal: controller.signal });
        if (controller.signal.aborted) return;
        if (operation.data?.state === "succeeded" && !operation.data.authorizationUrl) {
          setDisconnectOpen(false);
          setAction("idle");
          invalidateRepositories();
          refresh();
          return;
        }
        if (operation.data?.state === "failed") {
          setStartFailure(operation.data.error ? {
            apiVersion: operation.data.apiVersion,
            error: operation.data.error,
            code: operation.data.code,
            upstreamStatus: operation.data.upstreamStatus,
            detail: operation.data.detail,
          } : null);
          setAction("error");
          return;
        }
        if (operation.response?.status !== 202 || operation.data?.state !== "pending") {
          setStartFailure(connectionOperationError(operation.error));
          setAction(classifyConnectionMutationFailure(operation.response?.status, operation.error));
          return;
        }
        if (!await waitForConnectionPoll(controller.signal)) return;
      }
    } catch {
      if (!controller.signal.aborted) setAction("error");
    } finally {
      if (deadlineTimer !== undefined) clearTimeout(deadlineTimer);
      if (startController.current === controller) startController.current = null;
    }
  }

  const footer = status?.phase === "connected" ? (
    <div className="flex flex-wrap items-center justify-between gap-4">
      <p className="max-w-2xl text-[13px] text-muted-ink">Disconnecting revokes this connection for every current and future runtime using your identity.</p>
      <div className="flex gap-2">
        <button className="h-9 rounded-control border bg-panel px-3 text-sm font-semibold disabled:opacity-50" disabled={action === "working"} onClick={() => void connect()} type="button">Reconnect</button>
        <button className="h-9 rounded-control border border-err px-3 text-sm font-semibold text-err hover:bg-err-soft" onClick={() => setDisconnectOpen(true)} type="button">Disconnect…</button>
      </div>
      <ConfirmationDialog cancelLabel="Keep connection" confirmLabel="Disconnect GitHub" description="This revokes the shared connection for every current and future runtime using your identity." onConfirm={() => void disconnect()} onOpenChange={setDisconnectOpen} open={disconnectOpen} pending={action === "working"} title={`Disconnect ${connection?.displayName ?? "GitHub"}?`} />
    </div>
  ) : null;

  return (
    <SectionCard actions={<StatusBadge value={badge} />} footer={footer} title={<span className="flex items-center gap-3"><span aria-hidden="true" className="flex size-10 items-center justify-center rounded-[10px] bg-ink text-sm font-bold text-canvas">GH</span><span><span className="block text-[17px]">{connection?.displayName ?? "GitHub"}</span><span className="block text-xs font-normal text-muted-ink">{status?.accountId ? `${status.accountLogin ? `@${status.accountLogin} · ` : ""}GitHub ID ${status.accountId}` : status?.accountEmail ?? "Account not reported"}{status?.renewalCredentialExpiresAt ? ` · expires ${new Date(status.renewalCredentialExpiresAt).toLocaleDateString()}` : ""}</span></span></span>}>
      <div className="space-y-4">
        {status?.githubActionsIdentityLinked === true ? <p className="text-sm text-ok">GitHub Actions runs as you: linked.</p> : status?.githubActionsIdentityLinked === false ? <p className="text-sm text-err">GitHub Actions identity is not linked to this Steward user.</p> : null}
        <div><p className="mb-2 text-xs font-semibold text-muted-ink">Scopes</p>{status?.scopesGranted.length ? <ul className="flex flex-wrap gap-2">{status.scopesGranted.map((scope) => <li className="rounded-full bg-ok-soft px-3 py-1 font-mono text-xs" key={scope}>{scope} <span className="font-sans font-semibold text-ok">✓ granted</span></li>)}</ul> : <p className="text-sm text-muted-ink">No granted scopes reported.</p>}</div>
        {status?.scopesMissing.length ? <p className="text-sm text-err">Missing required scopes: {status.scopesMissing.join(", ")}.</p> : status ? <p className="text-sm text-ok">All required scopes granted.</p> : null}
        <dl className="grid gap-3 text-sm sm:grid-cols-2"><div><dt className="text-xs font-semibold text-muted-ink">Active credential expires</dt><dd className="mt-1">{status?.activeCredentialExpiresAt ?? "Not reported"}</dd></div><div><dt className="text-xs font-semibold text-muted-ink">Renewal credential expires</dt><dd className="mt-1">{status?.renewalCredentialExpiresAt ?? "Not reported"}</dd></div></dl>
        {!status ? <p className="text-sm text-muted-ink">Connection metadata is not currently available. Authorization can still be started safely.</p> : null}
        {status?.phase === "connected" ? reauthorizationRecommended ? <button className="min-h-10 rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={action === "working"} onClick={() => void connect()} type="button">Re-authorize GitHub</button> : null : <button className="min-h-10 rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={action === "working"} onClick={() => void connect()} type="button">{!status ? "Authorize / re-authorize GitHub" : status.phase === "reauth_required" || status.phase === "unavailable" ? "Re-authorize GitHub" : "Connect GitHub"}</button>}
        {action !== "idle" && action !== "working" ? <p className="text-sm text-err" role="alert">{action !== "orchestration-not-active" && startFailure ? connectionFailureMessage(startFailure) : { "orchestration-not-active": "Connections are disabled until task orchestration is active (stage 2).", "oauth-pending": "Finish or wait for the pending GitHub authorization before disconnecting.", "poll-expired": "The connection operation did not finish in time. Retry it; if it continues, contact an administrator.", conflict: "The connection changed before the action completed. Reload before retrying.", rejected: "Rust rejected the connection action.", forbidden: "The Rust authorization boundary rejected the connection action.", unavailable: "The authoritative connection service is unavailable.", error: "The server-owned connection action could not be completed." }[action]}</p> : null}
      </div>
    </SectionCard>
  );
}

function waitForConnectionPoll(signal: AbortSignal): Promise<boolean> {
  if (signal.aborted) return Promise.resolve(false);
  return new Promise((resolve) => {
    const finish = (ready: boolean) => {
      clearTimeout(timer);
      signal.removeEventListener("abort", abort);
      resolve(ready);
    };
    const abort = () => finish(false);
    const timer = setTimeout(() => finish(true), 1_000);
    signal.addEventListener("abort", abort, { once: true });
  });
}

function connectionOperationError(value: unknown): ConnectionOperationErrorResponse | null {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Partial<ConnectionOperationErrorResponse>;
  if (typeof candidate.apiVersion !== "string" || typeof candidate.error !== "string") return null;
  return {
    apiVersion: candidate.apiVersion,
    error: candidate.error,
    code: typeof candidate.code === "string" ? candidate.code : undefined,
    upstreamStatus: typeof candidate.upstreamStatus === "number" ? candidate.upstreamStatus : undefined,
    detail: typeof candidate.detail === "string" ? candidate.detail : undefined,
  };
}

function connectionFailureMessage(failure: ConnectionOperationErrorResponse): string {
  if (failure.code === "oauth_redirect_target_not_allowed") {
    return "MCP-GW rejected Steward's OAuth return origin. Add Steward's exact public origin to MCP-GW githubWrapper.oauth.redirectAfterAllowedOrigins.";
  }
  const messages: Record<string, string> = {
    runtime_authentication_failed: "The governed runtime could not authenticate to GitHub. Re-authorize GitHub; if it continues, ask an administrator to verify runtime credential injection.",
    proxy_policy_denied: "OpenShell policy denied the governed GitHub request. Ask an administrator to verify the runtime's GitHub proxy policy.",
    provider_authorization_failed: "MCP-GW rejected the governed runtime's authority. Ask an administrator to verify the runtime authority and MCP-GW configuration.",
    token_grant_failed: "The governed runtime could not receive its GitHub credential. Retry once; if it continues, ask an administrator to inspect MCP-GW token grants.",
    provider_response_invalid: "The provider returned a response Steward could not validate. Ask an administrator to verify the MCP-GW connection contract.",
    gateway_transport_failed: "The governed runtime could not reach MCP-GW. Ask an administrator to verify the gateway route and transport health.",
    gateway_status_invalid: "An older connection bridge reported an unexpected successful MCP-GW response. Ask an administrator to verify the deployed Steward and MCP-GW versions.",
    gateway_body_unavailable: "The governed runtime could not read MCP-GW's response body. Retry once; if it continues, ask an administrator to inspect gateway health.",
    gateway_unavailable: "MCP-GW is unavailable. Retry once; if it continues, ask an administrator to inspect the gateway service.",
    runtime_create_failed: "The governed connection runtime could not be created. Ask an administrator to inspect Steward runtime admission and controller events.",
    runtime_start_failed: "The governed connection runtime failed to start. Ask an administrator to inspect the AgentRuntime and OpenShell sandbox status.",
    connection_deadline_exceeded: "The governed connection did not become ready before its deadline. Retry once; if it continues, ask an administrator to inspect runtime health.",
    bridge_result_too_large: "The governed connection returned a result larger than Steward accepts for this operation. Ask an administrator to inspect the connection operation's failure category.",
  };
  const actionable = messages[failure.error];
  if (actionable) return actionable;
  return `GitHub authorization failed (${failure.error})${failure.detail ? `: ${failure.detail}` : "."}`;
}
