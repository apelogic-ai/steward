"use client";

import { DefinitionList, EmptyState, PageHeader, StatusBadge } from "@/components/workspace-ui";
import { useSession } from "@/session/session-context";

export function SettingsView({ admin = false }: Readonly<{ admin?: boolean }>) {
  const session = useSession();
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description={admin ? "Review the administrator identity and modes exposed only by the Rust session." : "Review the identity and mode exposed by the Rust session."} title="Settings" />
      {session.status === "authenticated" ? (
        <article className="space-y-5 rounded-panel border bg-panel p-6 shadow-sm">
          <div className="flex flex-wrap items-center justify-between gap-3"><h2 className="text-xl font-semibold">{admin ? "Administrator session" : "Server-owned session"}</h2><StatusBadge value={session.value.role} /></div>
          <DefinitionList items={[
            ["Display email", session.value.principal.displayEmail],
            ["Opaque user ID", session.value.principal.userId],
            ["Member roles", session.value.memberRoles.join(", ") || "None"],
            ["Available surfaces", session.value.surfaces.join(", ") || "None"],
          ]} />
          <p className="border-t pt-5 text-sm leading-6 text-muted-ink">Authentication, role resolution, and CSRF proof remain owned by Rust. This page does not persist identity data in browser storage.</p>
        </article>
      ) : <EmptyState title="Session unavailable"><p>The authoritative session is not available.</p></EmptyState>}
    </section>
  );
}
