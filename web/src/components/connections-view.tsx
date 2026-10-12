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
        <div className="flex items-center justify-between gap-4"><div><h2 className="text-lg font-semibold">Tools / MCP servers</h2><p className="text-sm text-muted-ink">Connect the tools your tasks can use.</p></div><button className="h-10 rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:cursor-not-allowed disabled:opacity-40" disabled={!canAdd} type="button">+ Add connection</button></div>
        {state.status === "ready" ? state.value.connections.map((connection) => <ProviderConnection connection={connection} key={connection.provider} refresh={() => setGeneration((value) => value + 1)} />) : <ProviderConnection metadataState={state.status} refresh={() => setGeneration((value) => value + 1)} />}
      </div>
      <div className="space-y-3">
        <div><h2 className="text-lg font-semibold">Models</h2><p className="text-sm text-muted-ink">Manage the API key used by tasks that call a model.</p></div>
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
        body: {} as never,
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
    return <SectionCard actions={<StatusBadge value={state.status === "loading" ? "Checking" : "Unavailable"} />} title="Models"><p className="text-sm text-muted-ink">Model connection details are not currently available.</p></SectionCard>;
  }
  const credential = value?.credential;
  if (value?.mode === "stock") {
    return <SectionCard actions={<StatusBadge value="Administrator managed" />} title="Models"><div className="space-y-4"><p className="text-sm text-muted-ink">API keys are managed by your Steward administrator. No personal API key is required.</p>{credential ? <><p className="text-sm">A saved key from {new Date(credential.savedAt).toLocaleString()} remains stored · key ending in <span className="font-mono">{credential.lastFour}</span></p><button className="h-10 rounded-control border border-err px-4 text-sm font-semibold text-err" onClick={() => setRemoving(true)} type="button">Remove stored API key…</button><ConfirmationDialog cancelLabel="Keep key" confirmLabel="Remove API key" description="This removes Steward's encrypted copy. It does not revoke or delete the key at the model provider." onConfirm={() => void remove()} onOpenChange={setRemoving} open={removing} pending={action === "working"} title="Remove stored model API key?" /></> : null}{action === "error" ? <p className="text-sm text-err" role="alert">The API key could not be removed. Try again.</p> : null}</div></SectionCard>;
  }
  return (
    <SectionCard actions={<StatusBadge value={credential ? "Configured" : "Key required"} />} title="Models">
      <div className="space-y-4">
        {credential ? <p className="text-sm">Saved {new Date(credential.savedAt).toLocaleString()} · key ending in <span className="font-mono">{credential.lastFour}</span></p> : <p className="text-sm text-muted-ink">Add your model API key before starting a task that uses a model. Runs started here or from GitHub Actions use this key.</p>}
        {editing ? <div className="space-y-3"><label className="block text-sm font-semibold" htmlFor="inference-api-key">Inference API key</label><input autoComplete="off" className="h-10 w-full max-w-xl rounded-control border bg-panel px-3 font-mono text-sm" id="inference-api-key" onChange={(event) => setKey(event.target.value)} type="password" value={key} /><div className="flex gap-2"><button className="h-10 rounded-control bg-brand px-4 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={action === "working" || key.length < 4} onClick={() => void save()} type="button">Save API key</button><button className="h-10 rounded-control border px-4 text-sm font-semibold" disabled={action === "working"} onClick={() => { setKey(""); setEditing(false); setAction("idle"); }} type="button">Cancel</button></div></div> : <div className="flex gap-2"><button className="h-10 rounded-control bg-brand px-4 text-sm font-semibold text-on-brand" onClick={() => setEditing(true)} type="button">{credential ? "Replace API key" : "Add API key"}</button>{credential ? <button className="h-10 rounded-control border border-err px-4 text-sm font-semibold text-err" onClick={() => setRemoving(true)} type="button">Remove API key…</button> : null}</div>}
        {action === "error" ? <p className="text-sm text-err" role="alert">The API key could not be saved. Check the key and try again.</p> : null}
        <ConfirmationDialog cancelLabel="Keep key" confirmLabel="Remove API key" description="This removes Steward's encrypted copy and blocks new model tasks. It does not revoke or delete the key at the model provider." onConfirm={() => void remove()} onOpenChange={setRemoving} open={removing} pending={action === "working"} title="Remove model API key?" />
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
      <p className="max-w-2xl text-[13px] text-muted-ink">Disconnecting removes GitHub access from every current and future run using your account.</p>
      <div className="flex gap-2">
        <button className="h-9 rounded-control border bg-panel px-3 text-sm font-semibold disabled:opacity-50" disabled={action === "working"} onClick={() => void connect()} type="button">Reconnect</button>
        <button className="h-9 rounded-control border border-err px-3 text-sm font-semibold text-err hover:bg-err-soft" onClick={() => setDisconnectOpen(true)} type="button">Disconnect…</button>
      </div>
      <ConfirmationDialog cancelLabel="Keep connection" confirmLabel="Disconnect GitHub" description="This removes GitHub access from every current and future run using your account." onConfirm={() => void disconnect()} onOpenChange={setDisconnectOpen} open={disconnectOpen} pending={action === "working"} title={`Disconnect ${connection?.displayName ?? "GitHub"}?`} />
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
        {action !== "idle" && action !== "working" ? <div className="text-sm text-err" role="alert"><p>{action !== "orchestration-not-active" && startFailure ? connectionFailureMessage(startFailure) : { "orchestration-not-active": "Connections are disabled until task runs are enabled for this installation.", "oauth-pending": "Finish or wait for the pending GitHub authorization before disconnecting.", "poll-expired": "The connection operation did not finish in time. Retry it; if it continues, contact an administrator.", conflict: "The connection changed before the action completed. Reload before retrying.", rejected: "The connection action was rejected.", forbidden: "You are not allowed to manage this connection.", unavailable: "Connections are unavailable. Try again.", error: "The connection action could not be completed." }[action]}</p>{action !== "orchestration-not-active" && startFailure ? <details className="mt-1 text-xs"><summary className="cursor-pointer">Details</summary><dl className="mt-1 grid grid-cols-[auto_1fr] gap-x-2"><dt>Category</dt><dd><code>{startFailure.error}</code></dd>{startFailure.code ? <><dt>Code</dt><dd><code>{startFailure.code}</code></dd></> : null}{startFailure.detail ? <><dt>Reason</dt><dd><code>{startFailure.detail}</code></dd></> : null}</dl></details> : null}</div> : null}
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
    return "GitHub couldn't return to this page. Ask an administrator to check the connection's return address.";
  }
  const messages: Record<string, string> = {
    runtime_authentication_failed: "GitHub couldn't verify this connection. Re-authorize GitHub. If it continues, ask an administrator to check the connection.",
    proxy_policy_denied: "This GitHub request isn't allowed by the current connection settings. Ask an administrator to review them.",
    provider_authorization_failed: "This connection doesn't permit the requested action. Ask an administrator to review its access.",
    token_grant_failed: "GitHub access couldn't be prepared. Retry once. If it continues, ask an administrator to check the connection.",
    provider_response_invalid: "GitHub returned an unexpected response. Retry once. If it continues, ask an administrator to check the connection.",
    gateway_transport_failed: "Steward couldn't reach GitHub. Retry once. If it continues, ask an administrator to check the connection.",
    gateway_status_invalid: "The connection returned an unexpected result. Ask an administrator to check the installed connection services.",
    gateway_body_unavailable: "Steward couldn't read GitHub's response. Retry once. If it continues, ask an administrator to check the connection.",
    gateway_unavailable: "GitHub connections are temporarily unavailable. Retry once. If it continues, ask an administrator to check the connection.",
    gateway_session_failed: "The GitHub connection couldn't be started. Retry once. If it continues, ask an administrator to check the connection.",
    runtime_create_failed: "The temporary GitHub connection couldn't be created. Retry once. If it continues, ask an administrator to check the connection.",
    runtime_start_failed: "The temporary GitHub connection couldn't start. Retry once. If it continues, ask an administrator to check the connection.",
    connection_deadline_exceeded: "The GitHub connection did not become ready in time. Retry once. If it continues, ask an administrator to check the connection.",
    bridge_result_too_large: "GitHub returned more data than this operation can process. Narrow the request and try again.",
  };
  const actionable = messages[failure.error];
  if (actionable) return actionable;
  return "GitHub authorization couldn't be completed. Retry once. If it continues, ask an administrator to check the connection.";
}
