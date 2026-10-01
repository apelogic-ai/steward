"use client";

import { useState } from "react";

import { updateBrowserPreferences } from "@/api-client";
import { SectionCard, StatusPill, ThemeControl } from "@/components/hs";
import { EmptyState, PageHeader } from "@/components/workspace-ui";
import { useSession } from "@/session/session-context";
import { setAdminSetupDismissed } from "@/data/admin-setup-preference";

export function SettingsView({ admin = false }: Readonly<{ admin?: boolean }>) {
  const session = useSession();
  const [guideState, setGuideState] = useState<"idle" | "working" | "done" | "error">("idle");
  return (
    <section aria-labelledby="page-title" className="space-y-6" data-workspace={admin ? "admin" : "user"}>
      <PageHeader description="Your identity and access as resolved by the HyperShell server." title="Settings" />
      {session.status === "authenticated" ? <>
        <SectionCard title={<span className="flex items-center gap-3"><span aria-hidden="true" className="flex size-11 items-center justify-center rounded-full bg-brand text-base font-bold text-on-brand">{(session.value.principal.displayName || session.value.principal.displayEmail).charAt(0).toUpperCase()}</span><span><span className="flex items-center gap-2 text-[17px]">{session.value.principal.displayName || session.value.principal.displayEmail}{session.value.role === "admin" ? <StatusPill tone="warn" value="Admin" /> : null}</span><span className="block text-xs font-normal text-muted-ink">{session.value.principal.displayEmail}</span></span></span>}>
          <dl className="grid gap-y-4 text-sm">
            <div className="grid gap-1 sm:grid-cols-[minmax(110px,200px)_1fr]"><dt className="font-semibold text-muted-ink">User ID</dt><dd className="break-all font-mono">{session.value.principal.userId}</dd></div>
            <div className="grid gap-1 sm:grid-cols-[minmax(110px,200px)_1fr]"><dt className="font-semibold text-muted-ink">Member roles</dt><dd className="flex flex-wrap gap-1.5">{session.value.memberRoles.length ? session.value.memberRoles.map((role) => <span className="rounded-md bg-line-soft px-2 py-1" key={role}>{role}</span>) : "None"}</dd></div>
            <div className="grid gap-1 sm:grid-cols-[minmax(110px,200px)_1fr]"><dt className="font-semibold text-muted-ink">Workspaces</dt><dd className="flex flex-wrap gap-1.5"><span className="rounded-md bg-line-soft px-2 py-1">User</span>{session.value.role === "admin" ? <span className="rounded-md bg-line-soft px-2 py-1">Admin</span> : null}</dd></div>
          </dl>
        </SectionCard>
        <SectionCard actions={<ThemeControl />} title="Mode"><div className="flex flex-wrap items-center justify-between gap-4"><p className="text-sm text-muted-ink">Follows your system setting by default.</p>{!admin ? <button className="rounded-control border px-3 py-2 text-sm font-semibold disabled:opacity-50" disabled={guideState === "working"} onClick={async () => {
          setGuideState("working");
          const result = await updateBrowserPreferences({ body: { onboardingDismissed: false }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf } });
          if (result.data && result.response?.ok) {
            setGuideState("done");
            window.dispatchEvent(new CustomEvent("hypershell:preferences-updated", { detail: { onboardingDismissed: false } }));
          } else setGuideState("error");
        }} type="button">{guideState === "done" ? "Guide is visible" : "Reopen Get started"}</button> : <button className="rounded-control border px-3 py-2 text-sm font-semibold" onClick={() => { setAdminSetupDismissed(false); setGuideState("done"); }} type="button">{guideState === "done" ? "Guide is visible" : "Reopen Get started"}</button>}</div>{guideState === "error" ? <p className="mt-3 text-sm text-err" role="alert">The onboarding preference could not be updated.</p> : null}</SectionCard>
        <p className="text-[13px] leading-5 text-muted-ink">Authentication, role resolution and CSRF proof are handled by the server. This page stores no identity data in the browser.</p>
      </> : <EmptyState title="Session unavailable"><p>The authoritative session is not available.</p></EmptyState>}
    </section>
  );
}
