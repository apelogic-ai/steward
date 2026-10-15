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
import { connectionHealth, connectionScopeStatus } from "@/components/connection-health";
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
  const connections = state.status === "ready" ? state.value.connections : [];
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Accounts your agents act through. HyperShell holds the credentials; agents never see them." title="Connections" />
      <div className="space-y-3">
        <div><h2 className="text-lg font-semibold">Tools / MCP servers</h2><p className="text-sm text-muted-ink">Connect the tools your tasks can use.</p></div>
        {state.status === "ready"
          ? connections.length > 0
            ? connections.map((connection) => <ProviderConnection connection={connection} key={connection.provider} refresh={() => setGeneration((value) => value + 1)} />)
            : <SectionCard actions={<StatusBadge value="Not configured" />} title="Tool connections"><p className="text-sm text-muted-ink">No tool providers are configured for this installation.</p></SectionCard>
          : <SectionCard actions={<StatusBadge value={state.status === "loading" ? "Checking" : "Unavailable"} />} title="Tool connections"><p className="text-sm text-muted-ink">Tool connection details are not currently available.</p></SectionCard>}
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

function ProviderConnection({ connection, refresh }: Readonly<{
  connection: ProviderConnectionView;
  refresh: () => void;
}>) {
  const session = useSession();
  const invalidateRepositories = useInvalidateGithubRepositories();
  const startController = useRef<AbortController | null>(null);
  const [disconnectOpen, setDisconnectOpen] = useState(false);
  const [action, setAction] = useState<"idle" | "working" | "poll-expired" | ConnectionMutationState>("idle");
  const [startFailure, setStartFailure] = useState<ConnectionOperationErrorResponse | null>(null);
  const status = connection.status;
  const providerName = providerDisplayName(connection);
  const health = connectionHealth(status);
  const scopeStatus = connectionScopeStatus(status);
  const canStart = connection.actions.includes("start");
  const canDisconnect = connection.actions.includes("disconnect");
  const needsReauthorization = scopeStatus === "missing" || health === "expiring_soon" || health === "expired" || health === "reauth_required";
  const badge = health === "expiring_soon" ? "Expiring soon" : health === "expired" ? "Credential expired" : status.phase;

  useEffect(() => () => startController.current?.abort(), []);

  async function connect() {
    if (session.status !== "authenticated") return;
    startController.current?.abort();
    const controller = new AbortController();
    startController.current = controller;
    setAction("working");
    setStartFailure(null);
    const provider = connection.provider;
    let deadlineTimer: ReturnType<typeof setTimeout> | undefined;
    try {
      const result = await startProviderConnection({ body: {}, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { provider }, signal: controller.signal });
      if (result.response?.status === 200 && result.data?.authorizationUrl) {
        window.location.assign(result.data.authorizationUrl);
        return;
      }
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
    const provider = connection.provider;
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
          if (provider === "github") invalidateRepositories();
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

  const disconnectDescription = `This removes ${providerName} access from every current and future run using your account.${connection.provider === "jira" ? " Also remove the app under Atlassian Connected Apps." : ""}`;
  const footer = canStart || canDisconnect ? <ProviderConnectionFooter
    canDisconnect={canDisconnect}
    canStart={canStart}
    connected={status.phase === "connected" || status.phase === "reauth_required"}
    disconnectDescription={disconnectDescription}
    disconnectOpen={disconnectOpen}
    needsReauthorization={needsReauthorization}
    onConnect={() => void connect()}
    onDisconnect={() => void disconnect()}
    onDisconnectOpenChange={setDisconnectOpen}
    providerId={connection.provider}
    providerName={providerName}
    working={action === "working"}
  /> : null;

  const accountIdentity = status?.accountId
    ? `${status.accountLogin ? `@${status.accountLogin} · ` : ""}${connection.provider === "github" ? "GitHub ID" : "Account ID"} ${status.accountId}`
    : status?.accountEmail ?? "Account not reported";

  return (
    <SectionCard actions={<StatusBadge value={badge} />} footer={footer} title={<span className="flex items-center gap-3"><span aria-hidden="true" className="flex size-10 items-center justify-center rounded-[10px] bg-ink text-sm font-bold text-canvas">{providerInitials(providerName)}</span><span><span className="block text-[17px]">{providerName}</span><span className="block text-xs font-normal text-muted-ink">{accountIdentity}{status?.renewalCredentialExpiresAt ? ` · expires ${formatCredentialDate(status.renewalCredentialExpiresAt)}` : ""}</span></span></span>}>
      <div className="space-y-4">
        {connection.provider === "github" && status.githubActionsIdentityLinked === true ? <p className="text-sm text-ok">GitHub Actions runs as you: linked.</p> : connection.provider === "github" && status.githubActionsIdentityLinked === false ? <p className="text-sm text-err">GitHub Actions identity is not linked to this Steward user.</p> : null}
        <div><p className="mb-2 text-xs font-semibold text-muted-ink">Scopes</p>{status.scopesGranted.length ? <ul className="flex flex-wrap gap-2">{status.scopesGranted.map((scope) => <li className="rounded-full bg-ok-soft px-3 py-1 font-mono text-xs" key={scope}>{scope} <span className="font-sans font-semibold text-ok">✓ granted</span></li>)}</ul> : <p className="text-sm text-muted-ink">No granted scopes reported.</p>}</div>
        {scopeStatus === "unavailable" ? <p className="text-sm text-muted-ink">{providerName} status couldn&apos;t be read right now.</p> : scopeStatus === "missing" ? <p className="text-sm text-err">Missing required scopes: {status.scopesMissing.join(", ")}.</p> : <p className="text-sm text-ok">All required scopes granted.</p>}
        <dl className="grid gap-3 text-sm sm:grid-cols-2"><div><dt className="text-xs font-semibold text-muted-ink">Active credential expires</dt><dd className="mt-1">{formatCredentialDate(status.activeCredentialExpiresAt)}</dd></div><div><dt className="text-xs font-semibold text-muted-ink">Renewal credential expires</dt><dd className="mt-1">{status.renewalCredentialExpiresAt ? formatCredentialDate(status.renewalCredentialExpiresAt) : status.phase === "connected" ? "Renews automatically" : "Not reported"}</dd></div></dl>
        {action !== "idle" && action !== "working" ? <div className="text-sm text-err" role="alert"><p>{action !== "orchestration-not-active" && startFailure ? connectionFailureMessage(startFailure, providerName) : { "orchestration-not-active": "Connections are disabled until task runs are enabled for this installation.", "oauth-pending": `Finish or wait for the pending ${providerName} authorization before disconnecting.`, "poll-expired": "The connection operation did not finish in time. Retry it; if it continues, contact an administrator.", conflict: "The connection changed before the action completed. Reload before retrying.", rejected: "The connection action was rejected.", forbidden: "You are not allowed to manage this connection.", unavailable: "Connections are unavailable. Try again.", error: "The connection action could not be completed." }[action]}</p>{action !== "orchestration-not-active" && startFailure ? <details className="mt-1 text-xs"><summary className="cursor-pointer">Details</summary><dl className="mt-1 grid grid-cols-[auto_1fr] gap-x-2"><dt>Category</dt><dd><code>{startFailure.error}</code></dd>{startFailure.code ? <><dt>Code</dt><dd><code>{startFailure.code}</code></dd></> : null}{startFailure.detail ? <><dt>Reason</dt><dd><code>{startFailure.detail}</code></dd></> : null}</dl></details> : null}</div> : null}
      </div>
    </SectionCard>
  );
}

function ProviderConnectionFooter({
  canDisconnect,
  canStart,
  connected,
  disconnectDescription,
  disconnectOpen,
  needsReauthorization,
  onConnect,
  onDisconnect,
  onDisconnectOpenChange,
  providerId,
  providerName,
  working,
}: Readonly<{
  canDisconnect: boolean;
  canStart: boolean;
  connected: boolean;
  disconnectDescription: string;
  disconnectOpen: boolean;
  needsReauthorization: boolean;
  onConnect: () => void;
  onDisconnect: () => void;
  onDisconnectOpenChange: (open: boolean) => void;
  providerId: string;
  providerName: string;
  working: boolean;
}>) {
  const emphasizeConnect = !connected || needsReauthorization;
  const explanation = canDisconnect
    ? `Disconnecting removes ${providerName} access from every current and future run using your account.`
    : needsReauthorization
      ? `Reconnect ${providerName} to restore access for approved tasks.`
      : connected
        ? `Reconnect ${providerName} to refresh access for approved tasks.`
        : `Connect ${providerName} to let approved tasks use your account.`;

  return (
    <div className="flex flex-wrap items-center justify-between gap-4" data-provider-action-footer={providerId}>
      <div className="max-w-2xl space-y-1 text-[13px] text-muted-ink">
        <p>{explanation}</p>
        {providerId === "jira" && canDisconnect ? <p>Also remove the app under Atlassian Connected Apps.</p> : null}
      </div>
      <div className="flex gap-2">
        {canStart ? <button className={emphasizeConnect ? "h-9 rounded-control bg-brand px-3 text-sm font-semibold text-on-brand disabled:opacity-50" : "h-9 rounded-control border bg-panel px-3 text-sm font-semibold disabled:opacity-50"} disabled={working} onClick={onConnect} type="button">{connected ? "Reconnect" : "Connect"}</button> : null}
        {canDisconnect ? <button className="h-9 rounded-control border border-err px-3 text-sm font-semibold text-err hover:bg-err-soft disabled:opacity-50" disabled={working} onClick={() => onDisconnectOpenChange(true)} type="button">Disconnect…</button> : null}
      </div>
      {canDisconnect ? <ConfirmationDialog cancelLabel="Keep connection" confirmLabel={`Disconnect ${providerName}`} description={disconnectDescription} onConfirm={onDisconnect} onOpenChange={onDisconnectOpenChange} open={disconnectOpen} pending={working} title={`Disconnect ${providerName}?`} /> : null}
    </div>
  );
}

function providerDisplayName(connection: ProviderConnectionView): string {
  const displayName = connection.displayName.trim();
  if (displayName) return displayName;
  const fallback = connection.provider
    .trim()
    .split(/[-_\s]+/)
    .filter(Boolean)
    .map((word) => `${word[0]?.toUpperCase() ?? ""}${word.slice(1)}`)
    .join(" ");
  return fallback || "Provider";
}

function formatCredentialDate(value: string | null | undefined): string {
  if (!value) return "Not reported";
  const parsed = new Date(value);
  return Number.isNaN(parsed.getTime()) ? "Not reported" : parsed.toLocaleString();
}

function providerInitials(displayName: string): string {
  const words = displayName.trim().split(/\s+/).filter(Boolean);
  if (words.length > 1) return words.slice(0, 2).map((word) => word[0]).join("").toUpperCase();
  return (words[0] ?? "?").slice(0, 2).toUpperCase();
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

function connectionFailureMessage(failure: ConnectionOperationErrorResponse, providerName: string): string {
  if (failure.code === "oauth_redirect_target_not_allowed") {
    return `${providerName} couldn't return to this page. Ask an administrator to check the connection's return address.`;
  }
  const messages: Record<string, string> = {
    runtime_authentication_failed: `${providerName} couldn't verify this connection. Re-authorize ${providerName}. If it continues, ask an administrator to check the connection.`,
    proxy_policy_denied: `This ${providerName} request isn't allowed by the current connection settings. Ask an administrator to review them.`,
    provider_authorization_failed: "This connection doesn't permit the requested action. Ask an administrator to review its access.",
    token_grant_failed: `${providerName} access couldn't be prepared. Retry once. If it continues, ask an administrator to check the connection.`,
    provider_response_invalid: `${providerName} returned an unexpected response. Retry once. If it continues, ask an administrator to check the connection.`,
    gateway_transport_failed: `Steward couldn't reach ${providerName}. Retry once. If it continues, ask an administrator to check the connection.`,
    gateway_status_invalid: "The connection returned an unexpected result. Ask an administrator to check the installed connection services.",
    gateway_body_unavailable: `Steward couldn't read ${providerName}'s response. Retry once. If it continues, ask an administrator to check the connection.`,
    gateway_unavailable: `${providerName} connections are temporarily unavailable. Retry once. If it continues, ask an administrator to check the connection.`,
    gateway_session_failed: `The ${providerName} connection couldn't be started. Retry once. If it continues, ask an administrator to check the connection.`,
    runtime_create_failed: `The temporary ${providerName} connection couldn't be created. Retry once. If it continues, ask an administrator to check the connection.`,
    runtime_start_failed: `The temporary ${providerName} connection couldn't start. Retry once. If it continues, ask an administrator to check the connection.`,
    connection_deadline_exceeded: `The ${providerName} connection did not become ready in time. Retry once. If it continues, ask an administrator to check the connection.`,
    bridge_result_too_large: `${providerName} returned more data than this operation can process. Narrow the request and try again.`,
  };
  const actionable = messages[failure.error];
  if (actionable) return actionable;
  return `${providerName} authorization couldn't be completed. Retry once. If it continues, ask an administrator to check the connection.`;
}
