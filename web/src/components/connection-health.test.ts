import { describe, expect, test } from "bun:test";

import type { ProviderConnectionStatus } from "@/api-client";
import { connectionHealth } from "./connection-health";

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

describe("GitHub credential health", () => {
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
});
