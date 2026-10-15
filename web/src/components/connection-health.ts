import type { ProviderConnectionStatus } from "@/api-client";

export type ConnectionHealth = "healthy" | "expiring_soon" | "expired" | "reauth_required" | "other";
export type ConnectionScopeStatus = "unavailable" | "missing" | "all_granted";

const REAUTH_WARNING_MS = 24 * 60 * 60 * 1000;

export function connectionHealth(status: ProviderConnectionStatus, now = Date.now()): ConnectionHealth {
  if (status.phase === "reauth_required") return "reauth_required";
  if (status.phase !== "connected") return "other";
  // An active credential may renew automatically. The renewal deadline, when reported,
  // is the date at which interactive authorization can actually become necessary.
  const deadline = status.renewalCredentialExpiresAt
    ?? (connectionRenewsAutomatically(status) ? null : status.activeCredentialExpiresAt);
  if (!deadline) return "healthy";
  const remaining = Date.parse(deadline) - now;
  if (!Number.isFinite(remaining) || remaining <= 0) return "expired";
  return remaining <= REAUTH_WARNING_MS ? "expiring_soon" : "healthy";
}

export function connectionRenewsAutomatically(status: ProviderConnectionStatus): boolean {
  return status.scopesGranted.includes("offline_access");
}

export function connectionScopeStatus(status: ProviderConnectionStatus): ConnectionScopeStatus {
  if (status.phase === "unavailable") {
    return "unavailable";
  }
  if (status.scopesMissing.length > 0) {
    return "missing";
  }
  if (
    status.scopesGranted.length === 0 ||
    status.scopesRequired.some((scope) => !status.scopesGranted.includes(scope))
  ) {
    return "unavailable";
  }
  return "all_granted";
}
