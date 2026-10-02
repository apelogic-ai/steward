import { describe, expect, test } from "bun:test";

import { breadcrumbsForPath, hasDualRole, isAccessPending, isActive, workspaceLandingPath } from "./app-shell";

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
});
