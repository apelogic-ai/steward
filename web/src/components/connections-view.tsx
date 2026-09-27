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
import { SectionCard } from "@/components/hs";
import { PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
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
  const [disconnectOpen, setDisconnectOpen] = useState(false);
  const [action, setAction] = useState<"idle" | "working" | "oauth-pending" | MutationFailureState>("idle");
  const status = connection?.status;
  const health = status ? connectionHealth(status) : undefined;
  const reauthorizationRecommended = health === "expiring_soon" || health === "expired";
  const badge = metadataState === "loading" ? "Checking" : !status ? "Status unavailable" : health === "expiring_soon" ? "Expiring soon" : health === "expired" ? "Credential expired" : status.phase;

  async function connect() {
    if (session.status !== "authenticated") return;
    setAction("working");
    const result = await startProviderConnection({ body: {}, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { provider: connection?.provider ?? "github" } });
    if (result.data?.authorizationUrl && result.response?.ok) { window.location.assign(result.data.authorizationUrl); return; }
    setAction(classifyMutationFailure(result.response?.status));
  }

  async function disconnect() {
    if (session.status !== "authenticated") return;
    setAction("working");
    const result = await disconnectProviderConnection({ body: { confirm: true }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf }, path: { provider: connection?.provider ?? "github" } });
    if (result.response?.status === 204) { setDisconnectOpen(false); setAction("idle"); refresh(); return; }
    if (result.response?.status === 409 && (result.error as { error?: string } | undefined)?.error === "oauth_flow_pending") { setAction("oauth-pending"); return; }
    setAction(classifyMutationFailure(result.response?.status));
  }

  const footer = status?.phase === "connected" ? (
    <div className="flex flex-wrap items-center justify-between gap-4">
      <p className="max-w-2xl text-[13px] text-muted-ink">Disconnecting revokes this connection for every current and future runtime using your identity.</p>
      <div className="flex gap-2">
        <button className="h-9 rounded-control border bg-panel px-3 text-sm font-semibold disabled:opacity-50" disabled={action === "working"} onClick={() => void connect()} type="button">Reconnect</button>
        <button className="h-9 rounded-control border border-err px-3 text-sm font-semibold text-err hover:bg-err-soft" onClick={() => setDisconnectOpen(true)} type="button">Disconnect…</button>
      </div>
      {disconnectOpen ? <div aria-modal="true" className="fixed inset-0 z-50 grid place-items-center bg-black/20 p-4" role="alertdialog"><div className="w-full max-w-sm rounded-card border bg-panel p-5 shadow-lg"><h2 className="text-base font-semibold">Disconnect {connection?.displayName ?? "GitHub"}?</h2><p className="mt-2 text-sm text-muted-ink">This revokes the shared connection for every current and future runtime using your identity.</p><div className="mt-5 flex justify-end gap-2"><button className="h-9 rounded-control border px-3 text-sm font-semibold" onClick={() => setDisconnectOpen(false)} type="button">Keep connection</button><button className="h-9 rounded-control bg-err px-3 text-sm font-semibold text-white disabled:opacity-50" disabled={action === "working"} onClick={() => void disconnect()} type="button">Disconnect GitHub</button></div></div></div> : null}
    </div>
  ) : null;

  return (
    <SectionCard actions={<StatusBadge value={badge} />} footer={footer} title={<span className="flex items-center gap-3"><span aria-hidden="true" className="flex size-10 items-center justify-center rounded-[10px] bg-ink text-sm font-bold text-canvas">GH</span><span><span className="block text-[17px]">{connection?.displayName ?? "GitHub"}</span><span className="block text-xs font-normal text-muted-ink">{status?.accountEmail ?? "Account not reported"}{status?.renewalCredentialExpiresAt ? ` · expires ${new Date(status.renewalCredentialExpiresAt).toLocaleDateString()}` : ""}</span></span></span>}>
      <div className="space-y-4">
        <div><p className="mb-2 text-xs font-semibold text-muted-ink">Scopes</p>{status?.scopesGranted.length ? <ul className="flex flex-wrap gap-2">{status.scopesGranted.map((scope) => <li className="rounded-full bg-ok-soft px-3 py-1 font-mono text-xs" key={scope}>{scope} <span className="font-sans font-semibold text-ok">✓ granted</span></li>)}</ul> : <p className="text-sm text-muted-ink">No granted scopes reported.</p>}</div>
        {status?.scopesMissing.length ? <p className="text-sm text-err">Missing required scopes: {status.scopesMissing.join(", ")}.</p> : status ? <p className="text-sm text-ok">All required scopes granted.</p> : null}
        <dl className="grid gap-3 text-sm sm:grid-cols-2"><div><dt className="text-xs font-semibold text-muted-ink">Active credential expires</dt><dd className="mt-1">{status?.activeCredentialExpiresAt ?? "Not reported"}</dd></div><div><dt className="text-xs font-semibold text-muted-ink">Renewal credential expires</dt><dd className="mt-1">{status?.renewalCredentialExpiresAt ?? "Not reported"}</dd></div></dl>
        {!status ? <p className="text-sm text-muted-ink">Connection metadata is not currently available. Authorization can still be started safely.</p> : null}
        {status?.phase === "connected" ? reauthorizationRecommended ? <button className="min-h-10 rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={action === "working"} onClick={() => void connect()} type="button">Re-authorize GitHub</button> : null : <button className="min-h-10 rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={action === "working"} onClick={() => void connect()} type="button">{!status ? "Authorize / re-authorize GitHub" : status.phase === "reauth_required" || status.phase === "unavailable" ? "Re-authorize GitHub" : "Connect GitHub"}</button>}
        {action !== "idle" && action !== "working" ? <p className="text-sm text-err" role="alert">{{ "oauth-pending": "Finish or wait for the pending GitHub authorization before disconnecting.", conflict: "The connection changed before the action completed. Reload before retrying.", rejected: "Rust rejected the connection action.", forbidden: "The Rust authorization boundary rejected the connection action.", unavailable: "The authoritative connection service is unavailable.", error: "The server-owned connection action could not be completed." }[action]}</p> : null}
      </div>
    </SectionCard>
  );
}
