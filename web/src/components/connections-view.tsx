"use client";

import { useCallback, useState } from "react";

import {
  disconnectProviderConnection,
  listProviderConnections,
  startProviderConnection,
  type ConnectionsCollectionResponse,
  type ProviderConnectionView,
} from "@/api-client";
import { connectionHealth } from "@/components/connection-health";
import { DefinitionList, PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { classifyMutationFailure, type MutationFailureState } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

export function ConnectionsView() {
  const [generation, setGeneration] = useState(0);
  const load = useCallback(() => {
    void generation;
    return listProviderConnections({ cache: "no-store", credentials: "same-origin" });
  }, [generation]);
  const state = useApiResource<ConnectionsCollectionResponse>(load);
  if (state.status === "forbidden" || state.status === "not-found") {
    return (
      <section aria-labelledby="page-title" className="space-y-6">
        <PageHeader description="View real provider status and initiate server-owned OAuth actions." title="Connections" />
        <ResourceBoundary state={state}>{() => null}</ResourceBoundary>
      </section>
    );
  }
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="View real provider status and initiate server-owned OAuth actions." title="Connections" />
      {state.status === "ready" ? state.value.connections.map((connection) => (
        <ProviderConnection connection={connection} key={connection.provider} refresh={() => setGeneration((value) => value + 1)} />
      )) : <ProviderConnection metadataState={state.status} refresh={() => setGeneration((value) => value + 1)} />}
      {state.status === "ready" && state.value.available.some((provider) => !provider.enabled) ? (
        <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold" disabled type="button">+ Add connection</button>
      ) : null}
    </section>
  );
}

function ProviderConnection({ connection, metadataState = "ready", refresh }: Readonly<{
  connection?: ProviderConnectionView;
  metadataState?: "loading" | "ready" | "unavailable" | "error";
  refresh: () => void;
}>) {
  const session = useSession();
  const [confirmDisconnect, setConfirmDisconnect] = useState(false);
  const [action, setAction] = useState<"idle" | "working" | "oauth-pending" | MutationFailureState>("idle");
  const status = connection?.status;
  const health = status ? connectionHealth(status) : undefined;
  const reauthorizationRecommended = health === "expiring_soon" || health === "expired";
  const badge = metadataState === "loading"
    ? "Checking"
    : !status
      ? "Status unavailable"
      : health === "expiring_soon"
        ? "Expiring soon"
        : health === "expired"
          ? "Credential expired"
          : status.phase;

  async function connect() {
    if (session.status !== "authenticated") return;
    setAction("working");
    const result = await startProviderConnection({
      body: {},
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
      path: { provider: connection?.provider ?? "github" },
    });
    if (result.data?.authorizationUrl && result.response?.ok) {
      window.location.assign(result.data.authorizationUrl);
      return;
    }
    setAction(classifyMutationFailure(result.response?.status));
  }

  async function disconnect() {
    if (session.status !== "authenticated" || !confirmDisconnect) return;
    setAction("working");
    const result = await disconnectProviderConnection({ body: { confirm: true }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { provider: connection?.provider ?? "github" } });
    if (result.response?.status === 204) {
      setConfirmDisconnect(false);
      setAction("idle");
      refresh();
      return;
    }
    if (result.response?.status === 409 && (result.error as { error?: string } | undefined)?.error === "oauth_flow_pending") {
      setAction("oauth-pending");
      return;
    }
    setAction(classifyMutationFailure(result.response?.status));
  }

  return (
    <article className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm">
      <div className="flex items-center justify-between gap-4"><div><h2 className="text-xl font-semibold">{connection?.displayName ?? "GitHub"}</h2><p className="mt-1 text-sm text-muted-ink">User-bound provider access</p></div><StatusBadge value={badge} /></div>
      <DefinitionList items={[
        ["Account", status?.accountEmail ?? "Not reported"],
        ["Required scopes", status ? status.scopesRequired.join(", ") || "None" : "Not reported"],
        ["Granted scopes", status ? status.scopesGranted.join(", ") || "None" : "Not reported"],
        ["Missing scopes", status ? status.scopesMissing.join(", ") || "None" : "Not reported"],
        ["Active credential expires", status?.activeCredentialExpiresAt ?? "Not reported"],
        ["Renewal credential expires", status?.renewalCredentialExpiresAt ?? "Not reported"],
      ]} />
      {!status ? <p className="text-sm text-muted-ink">Connection metadata is not currently available. Authorization can still be started safely.</p> : null}
      {status?.phase === "connected" ? (
        <div className="space-y-3 border-t pt-5">
          {reauthorizationRecommended ? <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50" disabled={action === "working"} onClick={() => void connect()} type="button">Re-authorize GitHub</button> : null}
          <p className="text-sm text-muted-ink">Disconnecting GitHub affects all present and future agent runtimes using the same Steward identity.</p>
          <label className="flex min-h-11 items-center gap-3 text-sm"><input checked={confirmDisconnect} onChange={(event) => setConfirmDisconnect(event.target.checked)} type="checkbox" />I understand this revokes the shared Steward connection.</label>
          <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={!confirmDisconnect || action === "working"} onClick={() => void disconnect()} type="button">Disconnect GitHub</button>
        </div>
      ) : (
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50" disabled={action === "working"} onClick={() => void connect()} type="button">{!status ? "Authorize / re-authorize GitHub" : status.phase === "reauth_required" || status.phase === "unavailable" ? "Re-authorize GitHub" : "Connect GitHub"}</button>
      )}
      {action !== "idle" && action !== "working" ? <p className="text-sm text-red-800" role="alert">{{ "oauth-pending": "Finish or wait for the pending GitHub authorization before disconnecting.", conflict: "The connection changed before the action completed. Reload before retrying.", rejected: "Rust rejected the connection action.", forbidden: "The Rust authorization boundary rejected the connection action.", unavailable: "The authoritative connection service is unavailable.", error: "The server-owned connection action could not be completed." }[action]}</p> : null}
    </article>
  );
}
