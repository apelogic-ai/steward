import { describe, expect, test } from "bun:test";

import type { ProviderConnectionStatus } from "@/api-client";
import { connectionHealth, connectionScopeStatus } from "./connection-health";

const now = Date.parse("2026-09-14T12:00:00.000Z");
const connected: ProviderConnectionStatus = {
  phase: "connected",
  accountEmail: "alice@example.com",
  scopesRequired: ["repo"],
  scopesGranted: ["repo"],
  scopesMissing: [],
  expiresAt: null,
  activeCredentialExpiresAt: "2026-09-14T13:00:00.000Z",
  renewalCredentialExpiresAt: "2026-09-20T12:00:00.000Z",
};

describe("provider credential health", () => {
  test("a renewable active credential stays green until its renewal deadline approaches", () => {
    expect(connectionHealth(connected, now)).toBe("healthy");
  });

  test("warning and expiry use the reported renewal deadline", () => {
    expect(connectionHealth({ ...connected, renewalCredentialExpiresAt: "2026-09-15T11:00:00.000Z" }, now)).toBe("expiring_soon");
    expect(connectionHealth({ ...connected, renewalCredentialExpiresAt: "2026-09-14T11:00:00.000Z" }, now)).toBe("expired");
  });

  test("unknown expiry is not fabricated and explicit reauthorization takes precedence", () => {
    expect(connectionHealth({ ...connected, activeCredentialExpiresAt: null, renewalCredentialExpiresAt: null }, now)).toBe("healthy");
    expect(connectionHealth({ ...connected, phase: "reauth_required" }, now)).toBe("reauth_required");
  });

  test("an expired active credential remains healthy when renewal happens on next use", () => {
    const renewalOnUse = {
      ...connected,
      activeCredentialExpiresAt: "2026-09-14T11:00:00.000Z",
      renewalCredentialExpiresAt: null,
    };

    expect(connectionHealth(renewalOnUse, now)).toBe("healthy");
    expect(connectionHealth({ ...renewalOnUse, renewalCredentialExpiresAt: "2026-09-14T11:00:00.000Z" }, now)).toBe("expired");
  });
});

describe("provider scope status", () => {
  test("no reported scopes never claim that every required scope is granted", () => {
    const status = {
      ...connected,
      phase: "connected" as const,
      scopesRequired: [],
      scopesGranted: [],
      scopesMissing: [],
    };

    expect(connectionScopeStatus(status)).not.toBe("all_granted");
    expect(connectionScopeStatus(status)).toBe("unavailable");
  });

  test("an unavailable status takes precedence over reported scopes", () => {
    expect(connectionScopeStatus({ ...connected, phase: "unavailable" })).toBe("unavailable");
  });

  test("reported scope outcomes remain missing or fully granted", () => {
    expect(connectionScopeStatus({ ...connected, scopesGranted: [], scopesMissing: ["repo"] })).toBe("missing");
    expect(connectionScopeStatus(connected)).toBe("all_granted");
  });

  test("partial scope evidence never claims every required scope is granted", () => {
    expect(
      connectionScopeStatus({
        ...connected,
        scopesRequired: ["repo", "workflow"],
        scopesGranted: ["repo"],
        scopesMissing: [],
      }),
    ).toBe("unavailable");
  });
});
