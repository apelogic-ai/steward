"use client";

import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react";

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
  const [status, setStatus] = useState<
    | { value: "loading" }
    | { value: "ready"; checks: AdminSetupCheck[] }
    | { value: "unavailable" }
  >({ value: "loading" });
  const dismissed = useSyncExternalStore(subscribeToAdminSetupPreference, adminSetupDismissed, adminSetupVisibleOnServer);
  const [refreshing, setRefreshing] = useState(false);
  const activeRequest = useRef<AbortController | null>(null);

  const refresh = useCallback(async () => {
    activeRequest.current?.abort();
    const controller = new AbortController();
    activeRequest.current = controller;
    setRefreshing(true);
    setStatus({ value: "loading" });
    try {
      const result = await getAdminSetupStatus({ cache: "no-store", credentials: "same-origin", signal: controller.signal });
      if (controller.signal.aborted) return;
      if (result.data && result.response?.ok) setStatus({ value: "ready", checks: result.data.checks });
      else setStatus({ value: "unavailable" });
    } catch {
      if (!controller.signal.aborted) setStatus({ value: "unavailable" });
    } finally {
      if (activeRequest.current === controller) {
        activeRequest.current = null;
        setRefreshing(false);
      }
    }
  }, []);

  useEffect(() => {
    if (dismissed) {
      activeRequest.current?.abort();
      activeRequest.current = null;
      return;
    }
    const initial = window.setTimeout(() => void refresh(), 0);
    const interval = window.setInterval(() => void refresh(), 15_000);
    return () => {
      window.clearTimeout(initial);
      window.clearInterval(interval);
      activeRequest.current?.abort();
      activeRequest.current = null;
    };
  }, [dismissed, refresh]);

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
      {status.value === "unavailable" ? <EmptyState title="Setup status is unavailable"><p role="alert">No prerequisite has been inferred. Retry when the authoritative service is available.</p></EmptyState> : null}
      {status.value === "loading" ? <EmptyState title="Loading setup status"><p role="status">Reading current server-owned prerequisites…</p></EmptyState> : null}
      {status.value === "ready" ? (
        <ol className="grid gap-4 md:grid-cols-2">
          {status.checks.map((item) => (
            <li key={item.id}>
              <SectionCard actions={<StatusPill tone={statusTone[item.status]} value={item.status.replaceAll("_", " ")} />} title={item.title}>
                <p className="text-sm leading-6 text-muted-ink">{item.detail}</p>
                <a className="mt-4 inline-flex text-sm font-semibold text-brand hover:underline" href={item.fixHref}>{item.status === "ready" ? "Review" : "How to fix"}</a>
                {item.optional ? <p className="mt-3 text-xs text-faint-ink">Optional</p> : null}
              </SectionCard>
            </li>
          ))}
        </ol>
      ) : null}
      <p className="text-xs leading-5 text-muted-ink">Hiding this guide is a presentation preference stored only in this browser. It does not change server state or authority.</p>
    </section>
  );
}
