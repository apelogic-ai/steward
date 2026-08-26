"use client";

import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import { useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from "react";

import { authStartPath } from "@/session/auth-redirect";
import { useSession, type SessionState } from "@/session/session-context";

const developerNavigation = [
  { href: "/envelopes", label: "Envelopes" },
  { href: "/runs", label: "Runs" },
  { href: "/connections", label: "Connections" },
  { href: "/settings", label: "Settings" },
] as const;

const adminNavigation = [
  { href: "/admin/workflows", label: "Workflows" },
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
    && session.value.role === "admin";
}

export function workspaceLandingPath(workspace: "admin" | "user"): string {
  return workspace === "admin" ? "/admin/envelopes/templates" : "/envelopes";
}

type Theme = "dark" | "light";

function preferredTheme(): Theme {
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function serverTheme(): Theme {
  return "light";
}

function subscribeToPreferredTheme(onChange: () => void): () => void {
  const preference = window.matchMedia("(prefers-color-scheme: dark)");
  preference.addEventListener("change", onChange);
  return () => preference.removeEventListener("change", onChange);
}

function signedOutPath(): string {
  return "/admin/sign-in";
}

function StatePanel({ children, title }: Readonly<{ children: ReactNode; title: string }>) {
  return (
    <section aria-labelledby="session-state-title" className="mx-auto mt-12 max-w-xl rounded-panel border bg-panel p-6 shadow-sm">
      <h1 className="text-xl font-semibold" id="session-state-title">{title}</h1>
      <div className="mt-3 text-sm leading-6 text-muted-ink">{children}</div>
    </section>
  );
}

function ThemeIcon({ theme }: Readonly<{ theme: Theme }>) {
  return theme === "light" ? (
    <svg aria-hidden="true" fill="none" height="20" viewBox="0 0 24 24" width="20">
      <circle cx="12" cy="12" r="4" stroke="currentColor" strokeWidth="2" />
      <path d="M12 2v2M12 20v2M4.93 4.93l1.42 1.42M17.66 17.66l1.41 1.41M2 12h2M20 12h2M4.93 19.07l1.42-1.42M17.66 6.34l1.41-1.41" stroke="currentColor" strokeLinecap="round" strokeWidth="2" />
    </svg>
  ) : (
    <svg aria-hidden="true" fill="none" height="20" viewBox="0 0 24 24" width="20">
      <path d="M20.2 15.3A8.5 8.5 0 0 1 8.7 3.8 8.5 8.5 0 1 0 20.2 15.3Z" stroke="currentColor" strokeLinejoin="round" strokeWidth="2" />
    </svg>
  );
}

function AccountMenu({ adminMode, session }: Readonly<{
  adminMode: boolean;
  session: Extract<SessionState, { status: "authenticated" }>;
}>) {
  const router = useRouter();
  const [open, setOpen] = useState(false);
  const [selectedTheme, setSelectedTheme] = useState<Theme | null>(null);
  const [logoutFailed, setLogoutFailed] = useState(false);
  const [loggingOut, setLoggingOut] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const displayEmail = session.value.principal.displayEmail;
  const verifiedDisplayName = session.value.principal.displayName?.trim();
  const displayName = verifiedDisplayName || displayEmail;
  const initial = displayName.trim().charAt(0).toUpperCase() || "?";
  const dualRole = hasDualRole(session);
  const systemTheme = useSyncExternalStore(subscribeToPreferredTheme, preferredTheme, serverTheme);
  const theme = selectedTheme ?? systemTheme;

  useEffect(() => {
    if (!open) return;

    const closeOnOutsidePointer = (event: PointerEvent) => {
      if (!containerRef.current?.contains(event.target as Node)) setOpen(false);
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      setOpen(false);
      buttonRef.current?.focus();
    };

    document.addEventListener("pointerdown", closeOnOutsidePointer);
    document.addEventListener("keydown", closeOnEscape);
    return () => {
      document.removeEventListener("pointerdown", closeOnOutsidePointer);
      document.removeEventListener("keydown", closeOnEscape);
    };
  }, [open]);

  useEffect(() => {
    if (selectedTheme) document.documentElement.dataset.theme = selectedTheme;
  }, [selectedTheme]);

  const logout = async () => {
    setLoggingOut(true);
    setLogoutFailed(false);
    try {
      const response = await fetch("/admin/auth/logout", {
        body: "{}",
        cache: "no-store",
        credentials: "same-origin",
        headers: {
          "Content-Type": "application/json",
          "X-Steward-CSRF": session.value.csrf,
        },
        method: "POST",
      });
      if (response.status === 204) {
        window.location.replace(signedOutPath());
        return;
      }
      if (response.status === 401) {
        window.location.replace(signedOutPath());
        return;
      }
    } catch {
      // The fixed failure message below keeps transport details out of the account surface.
    }
    setLoggingOut(false);
    setLogoutFailed(true);
  };

  return (
    <div className="account-menu-anchor relative" ref={containerRef}>
      <button
        aria-expanded={open}
        aria-haspopup="menu"
        aria-label="Account menu"
        className="account-avatar flex shrink-0 items-center justify-center border border-brand/50 bg-brand text-sm font-bold text-white shadow-sm transition hover:bg-brand-strong focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand"
        onClick={() => setOpen((value) => !value)}
        ref={buttonRef}
        title={displayName}
        type="button"
      >
        {initial}
      </button>
      {open ? (
        <div
          aria-label="Account"
          className="absolute end-0 top-12 z-40 w-72 overflow-hidden rounded-panel border bg-panel shadow-xl"
          role="menu"
        >
          <div className="flex items-center gap-3 px-4 py-4">
            <span aria-hidden="true" className="account-avatar account-avatar-large flex shrink-0 items-center justify-center bg-brand text-base font-bold text-white">
              {initial}
            </span>
            <div className="min-w-0">
              {verifiedDisplayName ? <p className="truncate text-sm font-semibold text-ink" title={verifiedDisplayName}>{verifiedDisplayName}</p> : null}
              <p className={`${verifiedDisplayName ? "mt-0.5 " : ""}truncate text-sm font-normal text-muted-ink`} title={displayEmail}>{displayEmail}</p>
            </div>
          </div>
          {dualRole ? (
            <div className="flex items-center justify-between gap-4 border-t px-4 py-3">
              <label className="text-sm font-medium text-ink" htmlFor="account-workspace">Workspace</label>
              <select
                aria-label="Workspace view"
                className="min-w-28 rounded-md border bg-canvas px-3 py-2 text-sm text-ink"
                id="account-workspace"
                onChange={(event) => {
                  setOpen(false);
                  router.push(workspaceLandingPath(event.target.value === "admin" ? "admin" : "user"));
                }}
                value={adminMode ? "admin" : "user"}
              >
                <option value="user">User</option>
                <option value="admin">Admin</option>
              </select>
            </div>
          ) : null}
          <div className="flex items-center justify-between border-t px-4 py-3">
            <span className="text-sm font-medium text-ink">Mode</span>
            <button
              aria-label={`Switch to ${theme === "light" ? "dark" : "light"} mode`}
              className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full border bg-canvas text-muted-ink transition hover:border-brand hover:text-brand"
              onClick={() => setSelectedTheme(theme === "light" ? "dark" : "light")}
              title={`${theme === "light" ? "Light" : "Dark"} mode`}
              type="button"
            >
              <ThemeIcon theme={theme} />
            </button>
          </div>
          <div className="border-t p-2">
            <button
              className="flex w-full justify-center rounded-md bg-brand px-3 py-2 text-sm font-semibold text-white shadow-sm hover:bg-brand-strong disabled:cursor-wait disabled:opacity-60"
              disabled={loggingOut}
              onClick={() => void logout()}
              type="button"
            >
              {loggingOut ? "Logging out…" : "Log out"}
            </button>
            {logoutFailed ? <p className="px-2 pt-2 text-center text-xs text-muted-ink" role="status">Could not log out.</p> : null}
          </div>
        </div>
      ) : null}
    </div>
  );
}

export function AppShell({ children }: Readonly<{ children: ReactNode }>) {
  const pathname = usePathname();
  const session = useSession();
  const adminMode = pathname.startsWith("/admin/");
  const navigation = adminMode ? adminNavigation : developerNavigation;
  const workspaceAuthorized = session.status === "authenticated"
    && (!adminMode || session.value.role === "admin");

  return (
    <div className="min-h-screen">
      <a className="sr-only focus:not-sr-only focus:fixed focus:start-4 focus:top-4 focus:z-50 focus:rounded-md focus:bg-panel focus:px-4 focus:py-3" href="#workspace">
        Skip to workspace
      </a>
      <header className="border-b bg-panel">
        <div className="mx-auto flex max-w-7xl flex-wrap items-center gap-x-7 gap-y-3 px-4 py-4 sm:px-6 lg:px-8">
          <Link
            aria-label="ApeLogic Steward home"
            className="me-auto flex items-center gap-2.5 text-lg font-semibold tracking-tight text-ink"
            href={adminMode && workspaceAuthorized ? "/admin/runs" : "/envelopes"}
          >
            {/* The same-origin SVG keeps the mark visible under Steward's strict style CSP. */}
            {/* eslint-disable-next-line @next/next/no-img-element */}
            <img alt="ApeLogic" className="apelogic-mark" height="32" src="/icon.svg" width="32" />
            <span>Steward</span>
          </Link>
          {workspaceAuthorized ? (
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
          {session.status === "authenticated" ? <AccountMenu adminMode={adminMode} session={session} /> : null}
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
              href={authStartPath(pathname)}
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
