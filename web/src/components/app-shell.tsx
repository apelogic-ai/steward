"use client";

import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import type { ReactNode } from "react";

import { useSession, type SessionState } from "@/session/session-context";

const developerNavigation = [
  { href: "/envelopes", label: "Envelopes" },
  { href: "/runs", label: "Runs" },
  { href: "/connections", label: "Connections" },
  { href: "/settings", label: "Settings" },
] as const;

const adminNavigation = [
  { href: "/admin/envelopes/templates", label: "Templates" },
  { href: "/admin/runs", label: "Runs" },
  { href: "/admin/approvals", label: "Approvals" },
  { href: "/admin/settings", label: "Settings" },
] as const;

export function isActive(pathname: string, href: string): boolean {
  return pathname === href || pathname.startsWith(`${href}/`);
}

export function hasDualRole(session: SessionState): boolean {
  return session.status === "authenticated"
    && session.value.role === "admin"
    && session.value.memberRoles.length > 0;
}

function loginReturnTo(pathname: string): string {
  if (["/connections", "/envelopes", "/envelopes/new", "/runs", "/settings"].includes(pathname)) {
    return pathname;
  }
  if (pathname.startsWith("/runs/")) {
    return "/runs";
  }
  return "/envelopes";
}

function StatePanel({ children, title }: Readonly<{ children: ReactNode; title: string }>) {
  return (
    <section aria-labelledby="session-state-title" className="mx-auto mt-12 max-w-xl rounded-panel border bg-panel p-6 shadow-sm">
      <h1 className="text-xl font-semibold" id="session-state-title">{title}</h1>
      <div className="mt-3 text-sm leading-6 text-muted-ink">{children}</div>
    </section>
  );
}

export function AppShell({ children }: Readonly<{ children: ReactNode }>) {
  const pathname = usePathname();
  const router = useRouter();
  const session = useSession();
  const adminMode = pathname.startsWith("/admin/");
  const navigation = adminMode ? adminNavigation : developerNavigation;
  const dualRole = hasDualRole(session);

  return (
    <div className="min-h-screen">
      <a className="sr-only focus:not-sr-only focus:fixed focus:start-4 focus:top-4 focus:z-50 focus:rounded-md focus:bg-panel focus:px-4 focus:py-3" href="#workspace">
        Skip to workspace
      </a>
      <header className="border-b bg-panel">
        <div className="mx-auto flex max-w-7xl flex-wrap items-center gap-x-7 gap-y-3 px-4 py-4 sm:px-6 lg:px-8">
          <Link className="me-auto text-lg font-semibold tracking-tight" href={adminMode ? "/admin/runs" : "/envelopes"}>
            Steward
          </Link>
          {session.status === "authenticated" ? (
            <nav aria-label="Primary navigation" className="order-3 flex w-full gap-1 overflow-x-auto sm:order-none sm:w-auto">
              {navigation.map(({ href, label }) => (
                <Link
                  aria-current={isActive(pathname, href) ? "page" : undefined}
                  className="rounded-md px-3 py-2 text-sm font-medium text-muted-ink hover:bg-canvas hover:text-ink aria-[current=page]:bg-canvas aria-[current=page]:text-brand-strong"
                  href={href}
                  key={href}
                >
                  {label}
                </Link>
              ))}
            </nav>
          ) : null}
          {dualRole ? (
            <label className="flex items-center gap-2 text-xs font-medium text-muted-ink">
              View
              <select
                aria-label="Workspace view"
                className="rounded-md border bg-panel px-2 py-1.5 text-sm text-ink"
                onChange={(event) => router.push(event.target.value === "admin" ? "/admin/runs" : "/envelopes")}
                value={adminMode ? "admin" : "developer"}
              >
                <option value="developer">Developer</option>
                <option value="admin">Admin</option>
              </select>
            </label>
          ) : null}
          <p aria-live="polite" className="max-w-52 truncate text-sm text-muted-ink">
            {session.status === "authenticated" ? session.value.principal.displayEmail : ""}
          </p>
        </div>
      </header>
      <main className="mx-auto max-w-7xl px-4 py-8 sm:px-6 lg:px-8" id="workspace">
        {session.status === "loading" ? (
          <StatePanel title="Loading Steward"><p>Checking the server-owned session…</p></StatePanel>
        ) : null}
        {session.status === "unauthorized" ? (
          <StatePanel title="Sign in required">
            <p>Your browser does not have a valid Steward session.</p>
            <a
              className="mt-5 inline-flex rounded-md bg-brand px-4 py-2 font-semibold text-white hover:bg-brand-strong"
              href={`/admin/auth/login?returnTo=${encodeURIComponent(loginReturnTo(pathname))}`}
            >
              Continue with Google
            </a>
          </StatePanel>
        ) : null}
        {session.status === "unavailable" ? (
          <StatePanel title="Session unavailable"><p>Steward could not reach the authoritative session service. Try again shortly.</p></StatePanel>
        ) : null}
        {session.status === "error" ? (
          <StatePanel title="Session error"><p>The session response was not accepted. No workspace data has been loaded.</p></StatePanel>
        ) : null}
        {session.status === "authenticated" && adminMode && session.value.role !== "admin" ? (
          <StatePanel title="Forbidden"><p>Your server-owned session does not grant administrator access.</p></StatePanel>
        ) : null}
        {session.status === "authenticated" && (!adminMode || session.value.role === "admin") ? children : null}
      </main>
    </div>
  );
}
