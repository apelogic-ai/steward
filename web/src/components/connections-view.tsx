"use client";

import { useCallback, useState } from "react";

import { connectionStatus, disconnectConnection, startConnection, type ConnectionStatusResponse } from "@/api-client";
import { DefinitionList, PageHeader, ResourceBoundary, StatusBadge } from "@/components/workspace-ui";
import { classifyMutationFailure, type MutationFailureState } from "@/data/mutation-state";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

export function ConnectionsView() {
  const [generation, setGeneration] = useState(0);
  const load = useCallback(() => {
    void generation;
    return connectionStatus({ cache: "no-store", credentials: "same-origin" });
  }, [generation]);
  const state = useApiResource<ConnectionStatusResponse>(load);
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="View real provider status and initiate server-owned OAuth actions." title="Connections" />
      <ResourceBoundary state={state}>{(connection) => <GithubConnection connection={connection} refresh={() => setGeneration((value) => value + 1)} />}</ResourceBoundary>
    </section>
  );
}

function GithubConnection({ connection, refresh }: Readonly<{ connection: ConnectionStatusResponse; refresh: () => void }>) {
  const session = useSession();
  const [confirmDisconnect, setConfirmDisconnect] = useState(false);
  const [action, setAction] = useState<"idle" | "working" | MutationFailureState>("idle");
  const status = connection.status;

  async function connect() {
    if (session.status !== "authenticated") return;
    setAction("working");
    const result = await startConnection({
      body: {},
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
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
    const result = await disconnectConnection({ body: { confirm: true }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf } });
    if (result.response?.status === 204) {
      setConfirmDisconnect(false);
      setAction("idle");
      refresh();
      return;
    }
    setAction(classifyMutationFailure(result.response?.status));
  }

  return (
    <article className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm">
      <div className="flex items-center justify-between gap-4"><div><h2 className="text-xl font-semibold">GitHub</h2><p className="mt-1 text-sm text-muted-ink">User-bound repository access</p></div><StatusBadge value={status.phase} /></div>
      <DefinitionList items={[
        ["Account", status.accountEmail ?? "Not connected"],
        ["Required scopes", status.scopesRequired.join(", ") || "None"],
        ["Granted scopes", status.scopesGranted.join(", ") || "None"],
        ["Missing scopes", status.scopesMissing.join(", ") || "None"],
        ["Expires", status.expiresAt ?? "Not reported"],
      ]} />
      {status.phase === "connected" ? (
        <div className="space-y-3 border-t pt-5">
          <label className="flex min-h-11 items-center gap-3 text-sm"><input checked={confirmDisconnect} onChange={(event) => setConfirmDisconnect(event.target.checked)} type="checkbox" />I understand this revokes the Steward connection.</label>
          <button className="min-h-11 rounded-md border px-4 py-2 text-sm font-semibold disabled:opacity-50" disabled={!confirmDisconnect || action === "working"} onClick={() => void disconnect()} type="button">Disconnect GitHub</button>
        </div>
      ) : (
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white disabled:opacity-50" disabled={action === "working" || status.phase === "unavailable"} onClick={() => void connect()} type="button">{status.phase === "reauth_required" ? "Reconnect GitHub" : "Connect GitHub"}</button>
      )}
      {action !== "idle" && action !== "working" ? <p className="text-sm text-red-800" role="alert">{{ conflict: "The connection changed before the action completed. Reload before retrying.", rejected: "Rust rejected the connection action.", forbidden: "The Rust authorization boundary rejected the connection action.", unavailable: "The authoritative connection service is unavailable.", error: "The server-owned connection action could not be completed." }[action]}</p> : null}
    </article>
  );
}
