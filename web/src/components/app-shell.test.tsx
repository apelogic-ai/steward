import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { renderToStaticMarkup } from "react-dom/server";

import { copyrightNotice } from "@/brand";
import { AccessPendingPanel, BuildInfoBlock, breadcrumbsForPath, hasDualRole, isAccessPending, isActive, signOut, workspaceLandingPath, type SignOutFetch } from "./app-shell";

const source = readFileSync(new URL("./app-shell.tsx", import.meta.url), "utf8");

describe("primary navigation", () => {
  test("selects Envelopes for list, new, detail, and nested run routes", () => {
    for (const pathname of [
      "/envelopes",
      "/envelopes/new",
      "/envelopes/00000000-0000-0000-0000-000000000001",
      "/envelopes/00000000-0000-0000-0000-000000000001/runs",
    ]) {
      expect(isActive(pathname, "/envelopes")).toBe(true);
    }
  });

  test("does not select a developer item for a similar or admin route", () => {
    expect(isActive("/envelopes-archive", "/envelopes")).toBe(false);
    expect(isActive("/admin/envelopes/templates", "/envelopes")).toBe(false);
    expect(isActive("/admin/settings", "/settings")).toBe(false);
  });

  test("keeps Task library and detail in the Tasks workspace", () => {
    expect(isActive("/tasks/steward%3Asha256%3Aexample", "/tasks")).toBe(true);
    expect(isActive("/tasks/steward%3Asha256%3Aexample", "/runs")).toBe(false);
  });

  test("offers My agents and selects it for list, creation, and thread routes", () => {
    expect(source).toContain('{ href: "/agents", label: "My agents" }');
    for (const pathname of ["/agents", "/agents/new", "/agents/agent-1"]) {
      expect(isActive(pathname, "/agents")).toBe(true);
    }
  });
});

describe("workspace mode availability", () => {
  test("shows both modes to an administrator without requiring a member role", () => {
    expect(hasDualRole({
      status: "authenticated",
      value: {
        apiVersion: "steward.browser-session/v1",
        csrf: "test-csrf",
        principal: { displayEmail: "alice@example.com", displayName: "Alice Example", userId: "usr_abcdef0123456789abcdef0123456789" },
        role: "admin",
        memberRoles: [],
        surfaces: ["agentRuns"],
      },
    })).toBe(true);
  });

  test("lands administrators on template authoring and members on envelopes", () => {
    expect(workspaceLandingPath("admin")).toBe("/admin/envelopes/templates");
    expect(workspaceLandingPath("user")).toBe("/envelopes");
  });

  test("holds a signed-in person without roles at access pending", () => {
    const session = {
      status: "authenticated" as const,
      value: {
        apiVersion: "steward.browser-session/v1",
        csrf: "test-csrf",
        principal: { displayEmail: "alice@example.com", displayName: "Alice Example", userId: "usr_abcdef0123456789abcdef0123456789" },
        role: "user" as const,
        memberRoles: [],
        surfaces: [],
      },
    };
    expect(isAccessPending(session, false)).toBe(true);
    expect(isAccessPending(session, true)).toBe(false);
  });
});

describe("breadcrumbs", () => {
  test("marks a dynamic run identifier as the current mono crumb", () => {
    expect(breadcrumbsForPath("/runs/task-3c9e1b27-long-value")).toEqual([
      { label: "User" },
      { href: "/runs", label: "Runs" },
      { label: "task-3c9e1b2…", mono: true },
    ]);
  });

  test("uses Requests as the administrator-facing approval label", () => {
    expect(breadcrumbsForPath("/admin/approvals")).toEqual([
      { label: "Admin" },
      { label: "Requests" },
    ]);
  });

  test("names agent list, creation, and thread routes", () => {
    expect(breadcrumbsForPath("/agents")).toEqual([
      { label: "User" },
      { label: "My agents" },
    ]);
    expect(breadcrumbsForPath("/agents/new")).toEqual([
      { label: "User" },
      { href: "/agents", label: "My agents" },
      { label: "New agent" },
    ]);
    expect(breadcrumbsForPath("/agents/agent-3c9e1b27-long-value")).toEqual([
      { label: "User" },
      { href: "/agents", label: "My agents" },
      { label: "agent-3c9e1b…", mono: true },
    ]);
  });
});

describe("access pending panel", () => {
  test("lets a signed-in person without a role sign out", () => {
    const html = renderToStaticMarkup(
      <AccessPendingPanel csrf="test-csrf" email="alice@example.com" userId="usr_abcdef0123456789abcdef0123456789" />,
    );
    expect(html).toContain("Access pending");
    expect(html).toContain("Copy user ID");
    expect(html).toContain(">Sign out<");
  });
});

describe("signing out", () => {
  type Call = { input: string; init: RequestInit | undefined };
  const fetchReturning = (status: number, calls: Call[]): SignOutFetch => (input, init) => {
    calls.push({ input, init });
    return Promise.resolve(new Response(null, { status }));
  };

  test("ends the session with CSRF proof and returns to sign-in", async () => {
    const calls: Call[] = [];
    const navigated: string[] = [];
    const signedOut = await signOut("test-csrf", { fetchImpl: fetchReturning(204, calls), navigate: (path) => navigated.push(path) });
    expect(signedOut).toBe(true);
    expect(calls).toHaveLength(1);
    expect(calls[0].input).toBe("/admin/auth/logout");
    expect(calls[0].init?.method).toBe("POST");
    expect(calls[0].init?.credentials).toBe("same-origin");
    expect(new Headers(calls[0].init?.headers).get("X-Steward-CSRF")).toBe("test-csrf");
    expect(navigated).toEqual(["/admin/sign-in"]);
  });

  test("treats an already-ended session as signed out", async () => {
    const navigated: string[] = [];
    expect(await signOut("test-csrf", { fetchImpl: fetchReturning(401, []), navigate: (path) => navigated.push(path) })).toBe(true);
    expect(navigated).toEqual(["/admin/sign-in"]);
  });

  test("reports a failure and stays on the page", async () => {
    const navigated: string[] = [];
    expect(await signOut("test-csrf", { fetchImpl: fetchReturning(503, []), navigate: (path) => navigated.push(path) })).toBe(false);
    const throwing: SignOutFetch = () => Promise.reject(new Error("offline"));
    expect(await signOut("test-csrf", { fetchImpl: throwing, navigate: (path) => navigated.push(path) })).toBe(false);
    expect(navigated).toEqual([]);
  });
});

describe("account menu build information", () => {
  const about = {
    buildTime: "2026-11-21T10:05:09Z",
    releaseCommit: "0123456789abcdef0123456789abcdef01234567",
    version: "1.2.3",
  };

  test("shows the product, copyright holder, version with release commit, and UTC build date", () => {
    const html = renderToStaticMarkup(<BuildInfoBlock about={about} />);
    expect(html).toContain("HyperShell Steward");
    expect(html).toContain("Copyright (c) 2026 ApeLogic Inc.");
    expect(html).toContain("Version: 1.2.3 (0123456789abcdef0123456789abcdef01234567)");
    expect(html).toContain("Build date 2026-11-21 10:05:09 UTC");
  });

  test("says when the build recorded no date", () => {
    const html = renderToStaticMarkup(<BuildInfoBlock about={{ ...about, buildTime: null }} />);
    expect(html).toContain("Version: 1.2.3 (0123456789abcdef0123456789abcdef01234567)");
    expect(html).toContain("Build date unavailable");
  });

  test("says when build information could not be loaded", () => {
    const html = renderToStaticMarkup(<BuildInfoBlock about={null} />);
    expect(html).toContain("HyperShell Steward");
    expect(html).toContain("Build information is unavailable.");
    expect(html).not.toContain("Version:");
  });

  test("takes the product name and copyright notice from the shared brand module", () => {
    expect(source).toContain('from "@/brand"');
    expect(source).not.toMatch(/Copyright \(c\)/);
  });

  test("keeps the copyright notice identical to the repository license", () => {
    const license = readFileSync(new URL("../../../LICENSE", import.meta.url), "utf8");
    const noticeLine = license.split("\n").find((line) => line.startsWith("Copyright (c)"));
    expect(noticeLine).toBe(copyrightNotice);
  });
});
