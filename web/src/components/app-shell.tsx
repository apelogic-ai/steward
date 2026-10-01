"use client";

import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import { useCallback, useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from "react";

import {
  getAdminRequestsSummary,
  getBrowserPreferences,
  updateBrowserPreferences,
  type BrowserTheme,
} from "@/api-client";
import { Breadcrumbs, type BreadcrumbItem } from "@/components/hs/breadcrumbs";
import { TooltipProvider } from "@/components/ui/tooltip";
import { authStartPath } from "@/session/auth-redirect";
import { deriveOnboardingProgress, loadOnboardingEvidence } from "@/data/onboarding-progress";
import { adminSetupDismissed, adminSetupVisibleOnServer, subscribeToAdminSetupPreference } from "@/data/admin-setup-preference";
import { useSession, type SessionState } from "@/session/session-context";

const userNavigation = [
  { href: "/get-started", label: "Get started" },
  { href: "/envelopes", label: "Envelopes" },
  { href: "/runs", label: "Runs" },
  { href: "/connections", label: "Connections" },
  { href: "/settings", label: "Settings" },
] as const;

const adminNavigation = [
  { href: "/admin/get-started", label: "Get started" },
  { href: "/admin/envelopes/templates", label: "Templates" },
  { href: "/admin/approvals", label: "Requests" },
  { href: "/admin/runs", label: "Runs" },
  { href: "/admin/workflows", label: "Workflows" },
  { href: "/admin/settings", label: "Settings" },
] as const;

export function isActive(pathname: string, href: string): boolean {
  if (href === "/admin/envelopes/templates" && pathname === "/admin/envelopes/provision") return true;
  return pathname === href || pathname.startsWith(`${href}/`);
}

export function hasDualRole(session: SessionState): boolean {
  return session.status === "authenticated" && session.value.role === "admin";
}

export function workspaceLandingPath(workspace: "admin" | "user"): string {
  return workspace === "admin" ? "/admin/envelopes/templates" : "/envelopes";
}

function shortId(value: string): string {
  return value.length > 16 ? `${value.slice(0, 12)}…` : value;
}

export function breadcrumbsForPath(pathname: string): BreadcrumbItem[] {
  const parts = pathname.split("/").filter(Boolean);
  const admin = parts[0] === "admin";
  const root: BreadcrumbItem = { label: admin ? "Admin" : "User" };
  const rest = admin ? parts.slice(1) : parts;
  if (rest.length === 0) return [root];

  if (admin && rest[0] === "envelopes" && rest[1] === "templates") {
    if (!rest[2]) return [root, { label: "Templates" }];
    if (rest[2] === "new") return [root, { href: "/admin/envelopes/templates", label: "Templates" }, { label: "New template" }];
    return [root, { href: "/admin/envelopes/templates", label: "Templates" }, { label: shortId(rest[2]), mono: true }];
  }
  if (admin && rest[0] === "envelopes" && rest[1] === "provision") {
    return [root, { href: "/admin/envelopes/templates", label: "Templates" }, { label: "Provision envelope" }];
  }
  if (rest[0] === "envelopes") {
    if (!rest[1]) return [root, { label: "Envelopes" }];
    if (rest[1] === "new") return [root, { href: "/envelopes", label: "Envelopes" }, { label: "New request" }];
    const items: BreadcrumbItem[] = [root, { href: "/envelopes", label: "Envelopes" }, { label: shortId(rest[1]), mono: true }];
    if (rest[2] === "runs") {
      items[items.length - 1] = { href: `/envelopes/${rest[1]}`, label: shortId(rest[1]), mono: true };
      items.push({ label: "Runs" });
    }
    return items;
  }
  if (rest[0] === "runs") {
    return rest[1]
      ? [root, { href: admin ? "/admin/runs" : "/runs", label: "Runs" }, { label: shortId(rest[1]), mono: true }]
      : [root, { label: "Runs" }];
  }
  if (admin && rest[0] === "approvals") {
    return rest[1]
      ? [root, { href: "/admin/approvals", label: "Requests" }, { label: shortId(rest[1]), mono: true }]
      : [root, { label: "Requests" }];
  }
  if (admin && rest[0] === "workflows") {
    const items: BreadcrumbItem[] = [root, { label: "Workflows" }];
    if (rest[1]) {
      items[1] = { href: "/admin/workflows", label: "Workflows" };
      items.push({ label: rest[1], mono: true });
    }
    return items;
  }
  const labels: Record<string, string> = {
    "get-started": "Get started",
    connections: "Connections",
    settings: "Settings",
  };
  return [root, { label: labels[rest[0]] ?? rest[0].replaceAll("-", " ") }];
}

type ResolvedTheme = "dark" | "light";

function preferredTheme(): ResolvedTheme {
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function serverTheme(): ResolvedTheme {
  return "light";
}

function subscribeToPreferredTheme(onChange: () => void): () => void {
  const preference = window.matchMedia("(prefers-color-scheme: dark)");
  preference.addEventListener("change", onChange);
  return () => preference.removeEventListener("change", onChange);
}

function applyThemePreference(preference: BrowserTheme): void {
  if (preference === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = preference;
  document.cookie = `hypershell-theme=${preference}; Path=/; Max-Age=31536000; SameSite=Lax`;
}

function StatePanel({ children, title }: Readonly<{ children: ReactNode; title: string }>) {
  return (
    <section aria-labelledby="session-state-title" className="mx-auto mt-12 max-w-xl rounded-card border bg-panel p-6">
      <h1 className="text-xl font-semibold" id="session-state-title">{title}</h1>
      <div className="mt-3 text-sm leading-6 text-muted-ink">{children}</div>
    </section>
  );
}

function WorkspaceSwitch({ adminMode }: Readonly<{ adminMode: boolean }>) {
  const router = useRouter();
  return (
    <div aria-label="Workspace view" className="grid grid-cols-2 rounded-control bg-line-soft p-[3px]" role="group">
      {(["user", "admin"] as const).map((workspace) => {
        const active = workspace === (adminMode ? "admin" : "user");
        return (
          <button
            aria-pressed={active}
            className={`h-8 rounded-[6px] text-[13px] font-semibold capitalize ${active ? "bg-panel text-ink shadow-control" : "text-muted-ink hover:text-ink"}`}
            key={workspace}
            onClick={() => router.push(workspaceLandingPath(workspace))}
            type="button"
          >
            {workspace}
          </button>
        );
      })}
    </div>
  );
}

function ProfileMenu({ adminMode, session }: Readonly<{
  adminMode: boolean;
  session: Extract<SessionState, { status: "authenticated" }>;
}>) {
  const [open, setOpen] = useState(false);
  const [selectedTheme, setSelectedTheme] = useState<BrowserTheme>("system");
  const [logoutFailed, setLogoutFailed] = useState(false);
  const [loggingOut, setLoggingOut] = useState(false);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const displayEmail = session.value.principal.displayEmail;
  const verifiedDisplayName = session.value.principal.displayName?.trim();
  const displayName = verifiedDisplayName || displayEmail;
  const initial = displayName.trim().charAt(0).toUpperCase() || "?";
  const systemTheme = useSyncExternalStore(subscribeToPreferredTheme, preferredTheme, serverTheme);
  const resolvedTheme = selectedTheme === "system" ? systemTheme : selectedTheme;

  useEffect(() => {
    let active = true;
    void getBrowserPreferences({ cache: "no-store", credentials: "same-origin" }).then((result) => {
      if (!active || !result.data || !result.response?.ok) return;
      const theme = result.data.theme ?? "system";
      setSelectedTheme(theme);
      applyThemePreference(theme);
    });
    return () => { active = false; };
  }, []);

  useEffect(() => {
    if (!open) return;
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      setOpen(false);
      buttonRef.current?.focus();
    };
    document.addEventListener("keydown", closeOnEscape);
    return () => document.removeEventListener("keydown", closeOnEscape);
  }, [open]);

  const chooseTheme = (theme: BrowserTheme) => {
    setSelectedTheme(theme);
    applyThemePreference(theme);
    void updateBrowserPreferences({
      body: { theme },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
    });
  };

  const logout = async () => {
    setLoggingOut(true);
    setLogoutFailed(false);
    try {
      const response = await fetch("/admin/auth/logout", {
        body: "{}",
        cache: "no-store",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json", "X-Steward-CSRF": session.value.csrf },
        method: "POST",
      });
      if (response.status === 204 || response.status === 401) {
        window.location.replace("/admin/sign-in");
        return;
      }
    } catch {
      // Fixed copy below deliberately avoids surfacing transport details.
    }
    setLoggingOut(false);
    setLogoutFailed(true);
  };

  return (
    <div className="overflow-hidden rounded-tile border border-line bg-panel">
      {open ? (
        <div aria-label="Account" className="space-y-3 border-b border-line-soft p-3" role="menu">
          <div>
            <p className="mb-1.5 text-[11px] font-semibold uppercase tracking-[0.06em] text-faint-ink">Mode</p>
            <div className="grid grid-cols-3 rounded-control bg-line-soft p-[3px]">
              {(["system", "light", "dark"] as const).map((theme) => (
                <button
                  aria-pressed={selectedTheme === theme}
                  className={`h-8 rounded-[6px] text-xs font-semibold capitalize ${selectedTheme === theme ? "bg-panel text-ink shadow-control" : "text-muted-ink hover:text-ink"}`}
                  key={theme}
                  onClick={() => chooseTheme(theme)}
                  type="button"
                >
                  {theme}{theme === "system" ? ` (${resolvedTheme})` : ""}
                </button>
              ))}
            </div>
          </div>
          <Link className="block rounded-control px-2 py-1.5 text-[13px] font-medium text-ink hover:bg-line-soft" href={adminMode ? "/admin/settings" : "/settings"} onClick={() => setOpen(false)}>Account settings</Link>
          <button className="w-full rounded-control px-2 py-1.5 text-left text-[13px] font-semibold text-err hover:bg-err-soft disabled:cursor-wait disabled:opacity-60" disabled={loggingOut} onClick={() => void logout()} type="button">
            {loggingOut ? "Logging out…" : "Log out"}
          </button>
          {logoutFailed ? <p className="text-xs text-err" role="status">Could not log out.</p> : null}
        </div>
      ) : null}
      <button
        aria-expanded={open}
        aria-haspopup="menu"
        aria-label="Account menu"
        className="flex w-full items-center gap-2.5 p-2.5 text-left hover:bg-subtle"
        onClick={() => setOpen((value) => !value)}
        ref={buttonRef}
        type="button"
      >
        <span aria-hidden="true" className="flex size-8 shrink-0 items-center justify-center rounded-full bg-brand text-[13px] font-bold text-on-brand">{initial}</span>
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[13px] font-semibold text-ink">{displayName}</span>
          <span className="block truncate text-xs text-muted-ink">{displayEmail}</span>
        </span>
        <span aria-hidden="true" className={`me-1 size-2 border-e border-b border-muted-ink transition-transform ${open ? "-rotate-[135deg] translate-y-0.5" : "rotate-45 -translate-y-0.5"}`} />
      </button>
    </div>
  );
}

function AppSidebar({ adminGuideDismissed, adminMode, mobile = false, needsAction, onNavigate, onboardingCompleted, onboardingDismissed, session }: Readonly<{
  adminGuideDismissed: boolean;
  adminMode: boolean;
  mobile?: boolean;
  needsAction: number | null;
  onNavigate?: () => void;
  onboardingCompleted: number | null;
  onboardingDismissed: boolean;
  session: Extract<SessionState, { status: "authenticated" }>;
}>) {
  const pathname = usePathname();
  const onboardingComplete = onboardingCompleted === 5;
  const navigation = adminMode ? adminNavigation.filter((item) => !adminGuideDismissed || item.href !== "/admin/get-started") : userNavigation.filter((item) => !(onboardingDismissed || onboardingComplete) || item.href !== "/get-started");
  return (
    <aside className={mobile ? "flex h-full w-[216px] flex-col border-e border-line bg-panel px-3 pt-[18px] pb-3 shadow-xl" : "sticky top-0 hidden h-screen flex-col border-e border-line bg-panel px-3 pt-[18px] pb-3 md:flex"}>
      <Link aria-label="HyperShell home" className="flex items-center gap-2.5 px-2 pb-[22px] text-ink" href={adminMode ? "/admin/envelopes/templates" : "/envelopes"} onClick={onNavigate}>
        {/* eslint-disable-next-line @next/next/no-img-element */}
        <img alt="HyperShell" className="size-8 rounded-control bg-white object-cover" height="32" src="/brand/logo" width="32" />
        <span className="text-[17px] font-semibold tracking-[-0.02em]">HyperShell</span>
      </Link>
      <p className="px-3 pb-1.5 text-[11px] font-semibold uppercase tracking-[0.06em] text-faint-ink">{adminMode ? "Admin workspace" : "User workspace"}</p>
      <nav aria-label="Primary navigation" className="space-y-0.5">
        {navigation.map(({ href, label }) => {
          const active = isActive(pathname, href);
          const count = href === "/admin/approvals" ? needsAction : href === "/get-started" && onboardingCompleted !== null ? `${onboardingCompleted}/5` : null;
          return (
            <Link
              aria-current={active ? "page" : undefined}
              className="flex min-h-9 items-center gap-2.5 rounded-control px-3 py-2 text-sm font-medium text-muted-ink hover:bg-line-soft hover:text-ink aria-[current=page]:bg-brand-soft aria-[current=page]:text-ink"
              href={href}
              key={href}
              onClick={onNavigate}
            >
              <span aria-hidden="true" className={`size-1.5 rounded-[2px] ${active ? "bg-brand" : "bg-field"}`} />
              <span>{label}</span>
              {count !== null && count !== 0 ? <span aria-hidden={href === "/admin/approvals" || undefined} className="ms-auto min-w-5 rounded-full bg-brand px-1.5 py-0.5 text-center text-[11px] font-bold text-on-brand">{count}</span> : null}
            </Link>
          );
        })}
      </nav>
      <div className="mt-4 space-y-2.5 md:mt-auto">
        {hasDualRole(session) ? <WorkspaceSwitch adminMode={adminMode} /> : null}
        <ProfileMenu adminMode={adminMode} session={session} />
      </div>
    </aside>
  );
}

export function AppShell({ children }: Readonly<{ children: ReactNode }>) {
  const pathname = usePathname();
  const session = useSession();
  const adminMode = pathname === "/admin" || pathname.startsWith("/admin/");
  const workspaceAuthorized = session.status === "authenticated" && (!adminMode || session.value.role === "admin");
  const [needsAction, setNeedsAction] = useState<number | null>(null);
  const adminGuideDismissed = useSyncExternalStore(subscribeToAdminSetupPreference, adminSetupDismissed, adminSetupVisibleOnServer);
  const [onboardingCompleted, setOnboardingCompleted] = useState<number | null>(null);
  const [onboardingDismissed, setOnboardingDismissed] = useState(false);
  const [mobileMenuOpen, setMobileMenuOpen] = useState(false);
  const mobileMenuButtonRef = useRef<HTMLButtonElement>(null);
  const mobileNavigationRef = useRef<HTMLDivElement>(null);

  const closeMobileMenu = useCallback(() => {
    setMobileMenuOpen(false);
    requestAnimationFrame(() => mobileMenuButtonRef.current?.focus());
  }, []);

  useEffect(() => {
    if (!mobileMenuOpen) return;
    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") closeMobileMenu();
    };
    document.addEventListener("keydown", closeOnEscape);
    requestAnimationFrame(() => mobileNavigationRef.current?.querySelector<HTMLElement>("a")?.focus());
    return () => {
      document.removeEventListener("keydown", closeOnEscape);
      document.body.style.overflow = previousOverflow;
    };
  }, [closeMobileMenu, mobileMenuOpen]);

  useEffect(() => {
    if (!adminMode || !workspaceAuthorized) return;
    let active = true;
    void getAdminRequestsSummary({ cache: "no-store", credentials: "same-origin" }).then((result) => {
      if (active && result.data && result.response?.ok) setNeedsAction(result.data.needsAction);
    });
    return () => { active = false; };
  }, [adminMode, workspaceAuthorized]);

  useEffect(() => {
    if (adminMode || !workspaceAuthorized) return;
    let active = true;
    const refreshProgress = () => void loadOnboardingEvidence().then((result) => {
      if (active && result.data && result.response?.ok) {
        setOnboardingDismissed(result.data.preferences.onboardingDismissed);
        setOnboardingCompleted(deriveOnboardingProgress(result.data).completed);
      }
    });
    refreshProgress();
    const preferencesUpdated = (event: Event) => {
      if (event instanceof CustomEvent && typeof event.detail?.onboardingDismissed === "boolean") {
        setOnboardingDismissed(event.detail.onboardingDismissed);
      }
      refreshProgress();
    };
    window.addEventListener("hypershell:preferences-updated", preferencesUpdated);
    return () => {
      active = false;
      window.removeEventListener("hypershell:preferences-updated", preferencesUpdated);
    };
  }, [adminMode, workspaceAuthorized]);

  return (
    <TooltipProvider>
      <div className="min-h-screen md:grid md:grid-cols-[216px_minmax(0,1fr)]">
        <a className="sr-only focus:not-sr-only focus:fixed focus:start-4 focus:top-4 focus:z-50 focus:rounded-control focus:bg-panel focus:px-4 focus:py-3" href="#workspace">Skip to workspace</a>
        {session.status === "authenticated" && workspaceAuthorized ? <AppSidebar adminGuideDismissed={adminGuideDismissed} adminMode={adminMode} needsAction={needsAction} onboardingCompleted={onboardingCompleted} onboardingDismissed={onboardingDismissed} session={session} /> : <div className="hidden md:block" />}
        {session.status === "authenticated" && workspaceAuthorized ? <header className="sticky top-0 z-30 flex h-14 items-center justify-between border-b border-line bg-panel px-4 md:hidden"><Link aria-label="HyperShell home" className="flex items-center gap-2 text-ink" href={adminMode ? "/admin/envelopes/templates" : "/envelopes"}>{/* eslint-disable-next-line @next/next/no-img-element */}
        <img alt="HyperShell" className="size-7 rounded-control bg-white object-cover" height="28" src="/brand/logo" width="28" /><span className="text-base font-semibold">HyperShell</span></Link><button aria-controls="mobile-navigation" aria-expanded={mobileMenuOpen} aria-label="Open navigation" className="grid size-10 place-items-center rounded-control border bg-panel text-xl" onClick={() => setMobileMenuOpen(true)} ref={mobileMenuButtonRef} type="button">☰</button></header> : null}
        {mobileMenuOpen && session.status === "authenticated" && workspaceAuthorized ? <div aria-label="Navigation" aria-modal="true" className="fixed inset-0 z-50 flex bg-black/30 md:hidden" id="mobile-navigation" ref={mobileNavigationRef} role="dialog"><AppSidebar adminGuideDismissed={adminGuideDismissed} adminMode={adminMode} mobile needsAction={needsAction} onNavigate={closeMobileMenu} onboardingCompleted={onboardingCompleted} onboardingDismissed={onboardingDismissed} session={session} /><button aria-label="Close navigation" className="flex-1" onClick={closeMobileMenu} type="button" /></div> : null}
        <main className="min-w-0 px-4 pt-[18px] pb-16 sm:px-7" id="workspace">
          <div className="mx-auto max-w-[1180px]">
            {session.status === "loading" ? <StatePanel title="Loading HyperShell"><p>Checking the server-owned session…</p></StatePanel> : null}
            {session.status === "unauthorized" ? (
              <StatePanel title="Sign in required">
                <p>Your browser does not have a valid HyperShell session.</p>
                <a className="mt-5 inline-flex h-10 items-center rounded-control bg-brand px-4 text-sm font-semibold text-on-brand hover:bg-brand-hover" href={authStartPath(pathname)}>Continue with Google</a>
              </StatePanel>
            ) : null}
            {session.status === "unavailable" ? <StatePanel title="Session unavailable"><p>HyperShell could not reach the authoritative session service. Try again shortly.</p></StatePanel> : null}
            {session.status === "error" ? <StatePanel title="Session error"><p>The session response was not accepted. No workspace data has been loaded.</p></StatePanel> : null}
            {session.status === "authenticated" && adminMode && session.value.role !== "admin" ? <StatePanel title="Forbidden"><p>Your server-owned session does not grant administrator access.</p></StatePanel> : null}
            {session.status === "authenticated" && workspaceAuthorized ? (
              <>
                <Breadcrumbs items={breadcrumbsForPath(pathname)} />
                {children}
              </>
            ) : null}
          </div>
        </main>
      </div>
    </TooltipProvider>
  );
}
