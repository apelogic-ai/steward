"use client";

import { useCallback, useEffect, useRef, useState } from "react";

import {
  disconnectProviderConnection,
  getProviderConnectionStartOperation,
  listProviderConnections,
  startProviderConnection,
  type ConnectionOperationErrorResponse,
  type ConnectionsCollectionResponse,
  type ProviderConnectionView,
} from "@/api-client";
import { connectionHealth } from "@/components/connection-health";
import { classifyConnectionMutationFailure, type ConnectionMutationState } from "@/components/connection-mutation-state";
import { ConfirmationDialog, SectionCard } from "@/components/hs";
import { PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

const MAX_CONNECTION_POLL_MS = 60_000;

export function ConnectionsView() {
  const [generation, setGeneration] = useState(0);
  const load = useCallback(() => {
    void generation;
    return listProviderConnections({ cache: "no-store", credentials: "same-origin" });
  }, [generation]);
  const state = useApiResource<ConnectionsCollectionResponse>(load);
  if (state.status === "forbidden" || state.status === "not-found") {
    return <section aria-labelledby="page-title" className="space-y-6"><PageHeader description="Accounts your agents act through. HyperShell holds the credentials; agents never see them." title="Connections" /><ResourceBoundary state={state}>{() => null}</ResourceBoundary></section>;
  }
  const canAdd = state.status === "ready" && state.value.available.some((candidate) => candidate.enabled
    && !state.value.connections.some((connection) => connection.provider === candidate.provider));
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader actions={<button className="h-10 rounded-control bg-brand px-4 text-sm font-semibold text-on-brand disabled:cursor-not-allowed disabled:opacity-40" disabled={!canAdd} type="button">+ Add connection</button>} description="Accounts your agents act through. HyperShell holds the credentials; agents never see them." title="Connections" />
      {state.status === "ready" ? state.value.connections.map((connection) => <ProviderConnection connection={connection} key={connection.provider} refresh={() => setGeneration((value) => value + 1)} />) : <ProviderConnection metadataState={state.status} refresh={() => setGeneration((value) => value + 1)} />}
    </section>
  );
}

function ProviderConnection({ connection, metadataState = "ready", refresh }: Readonly<{
  connection?: ProviderConnectionView;
  metadataState?: "loading" | "ready" | "unavailable" | "error";
  refresh: () => void;
}>) {
  const session = useSession();
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
        if (operation.data?.state === "succeeded" && operation.data.authorizationUrl) {
          window.location.assign(operation.data.authorizationUrl);
          return;
        }
        if (operation.data?.state === "failed") {
          setStartFailure(operation.data.error ? {
            apiVersion: operation.data.apiVersion,
            error: operation.data.error,
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
    setAction("working");
    const result = await disconnectProviderConnection({ body: { confirm: true }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { provider: connection?.provider ?? "github" } });
    if (result.response?.status === 204) { setDisconnectOpen(false); setAction("idle"); refresh(); return; }
    setAction(classifyConnectionMutationFailure(result.response?.status, result.error));
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
        {action !== "idle" && action !== "working" ? <p className="text-sm text-err" role="alert">{action !== "orchestration-not-active" && startFailure ? `GitHub authorization failed (${startFailure.error})${startFailure.detail ? `: ${startFailure.detail}` : "."}` : { "orchestration-not-active": "Connections are disabled until task orchestration is active (stage 2).", "oauth-pending": "Finish or wait for the pending GitHub authorization before disconnecting.", "poll-expired": "Authorization did not become ready in time. Retry the connection; if it continues, contact an administrator.", conflict: "The connection changed before the action completed. Reload before retrying.", rejected: "Rust rejected the connection action.", forbidden: "The Rust authorization boundary rejected the connection action.", unavailable: "The authoritative connection service is unavailable.", error: "The server-owned connection action could not be completed." }[action]}</p> : null}
      </div>
    </SectionCard>
  );
}

function boundedConnectionPollDeadline(value: string): number | null {
  const parsed = Date.parse(value);
  if (!Number.isFinite(parsed)) return null;
  return Math.min(parsed, Date.now() + MAX_CONNECTION_POLL_MS);
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
    upstreamStatus: typeof candidate.upstreamStatus === "number" ? candidate.upstreamStatus : undefined,
    detail: typeof candidate.detail === "string" ? candidate.detail : undefined,
  };
}
