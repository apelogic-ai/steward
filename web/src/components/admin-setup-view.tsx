"use client";

import { useCallback, useEffect, useState, useSyncExternalStore } from "react";

import { getAdminSetupStatus, type AdminSetupCheck } from "@/api-client";
import { SectionCard, StatusPill } from "@/components/hs";
import { EmptyState, PageHeader } from "@/components/workspace-ui";
import {
  adminSetupDismissed,
  adminSetupVisibleOnServer,
  setAdminSetupDismissed,
  subscribeToAdminSetupPreference,
} from "@/data/admin-setup-preference";

const statusTone = {
  attention: "warn",
  not_configured: "neutral",
  ready: "ok",
  unknown: "neutral",
} as const;

export function AdminSetupView() {
  const [checks, setChecks] = useState<AdminSetupCheck[] | null>(null);
  const dismissed = useSyncExternalStore(subscribeToAdminSetupPreference, adminSetupDismissed, adminSetupVisibleOnServer);
  const [failed, setFailed] = useState(false);
  const [refreshing, setRefreshing] = useState(false);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    const result = await getAdminSetupStatus({ cache: "no-store", credentials: "same-origin" });
    if (result.data && result.response?.ok) {
      setChecks(result.data.checks);
      setFailed(false);
    } else setFailed(true);
    setRefreshing(false);
  }, []);

  useEffect(() => {
    const initial = window.setTimeout(() => void refresh(), 0);
    const interval = window.setInterval(() => void refresh(), 15_000);
    return () => {
      window.clearTimeout(initial);
      window.clearInterval(interval);
    };
  }, [refresh]);

  if (dismissed) {
    return <EmptyState title="The administrator setup guide is hidden in this browser."><button className="mt-3 rounded-control border px-3 py-2 font-semibold" onClick={() => setAdminSetupDismissed(false)} type="button">Show guide again</button></EmptyState>;
  }

  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader
        actions={<div className="flex gap-2"><button className="rounded-control border px-3 py-2 text-sm font-semibold disabled:opacity-50" disabled={refreshing} onClick={() => void refresh()} type="button">{refreshing ? "Refreshing…" : "Refresh status"}</button><button className="rounded-control border px-3 py-2 text-sm font-semibold" onClick={() => setAdminSetupDismissed(true)} type="button">Hide this guide</button></div>}
        description="Live, read-only checks for the prerequisites behind governed agent runs. Optional checks do not broaden authority."
        title="Get started"
      />
      {failed ? <p className="rounded-control border border-err/40 bg-err/10 p-3 text-sm text-err" role="alert">Setup status is unavailable. No prerequisite has been inferred.</p> : null}
      {checks === null ? <EmptyState title="Loading setup status"><p role="status">Reading current server-owned prerequisites…</p></EmptyState> : (
        <ol className="grid gap-4 md:grid-cols-2">
          {checks.map((item) => (
            <li key={item.id}>
              <SectionCard actions={<StatusPill tone={statusTone[item.status]} value={item.status.replaceAll("_", " ")} />} title={item.title}>
                <p className="text-sm leading-6 text-muted-ink">{item.detail}</p>
                <a className="mt-4 inline-flex text-sm font-semibold text-brand hover:underline" href={item.fixHref}>{item.status === "ready" ? "Review" : "How to fix"}</a>
                {item.optional ? <p className="mt-3 text-xs text-faint-ink">Optional</p> : null}
              </SectionCard>
            </li>
          ))}
        </ol>
      )}
      <p className="text-xs leading-5 text-muted-ink">Hiding this guide is a presentation preference stored only in this browser. It does not change server state or authority.</p>
    </section>
  );
}
