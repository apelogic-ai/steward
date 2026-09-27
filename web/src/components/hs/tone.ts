import type { AdminRequestState, EnvelopeRequestStatus, TaskPhase } from "@/api-client";

export type StatusTone = "ok" | "err" | "warn" | "info" | "neutral";

const taskPhaseTones = {
  submitted: "info",
  parked: "warn",
  queued: "info",
  running: "info",
  succeeded: "ok",
  failed: "err",
  cancelled: "neutral",
} satisfies Record<TaskPhase, StatusTone>;

const envelopeRequestTones = {
  pending: "warn",
  approved: "ok",
  rejected: "err",
  provisioned: "ok",
  stale: "neutral",
  conflict: "err",
} satisfies Record<EnvelopeRequestStatus, StatusTone>;

const adminRequestTones = {
  requested: "warn",
  escalated: "err",
  auto_approved: "ok",
  approved: "ok",
  rejected: "neutral",
  expired: "neutral",
} satisfies Record<AdminRequestState, StatusTone>;

export function toneForTaskPhase(value: TaskPhase): StatusTone {
  return taskPhaseTones[value];
}

export function toneForEnvelopeRequest(value: EnvelopeRequestStatus): StatusTone {
  return envelopeRequestTones[value];
}

export function toneForAdminRequest(value: AdminRequestState): StatusTone {
  return adminRequestTones[value];
}

export function toneForStatus(value: string): StatusTone {
  const normalized = value.toLowerCase();
  if (normalized in taskPhaseTones) return taskPhaseTones[normalized as TaskPhase];
  if (normalized in envelopeRequestTones) return envelopeRequestTones[normalized as EnvelopeRequestStatus];
  if (normalized in adminRequestTones) return adminRequestTones[normalized as AdminRequestState];
  if (normalized === "connected") return "ok";
  if (normalized === "credential expired" || normalized === "reauth_required") return "err";
  if (normalized === "expiring soon") return "warn";
  return "neutral";
}

export function displayStatus(value: string): string {
  return value.toLowerCase() === "auto_approved"
    ? "Auto-approved"
    : value.replaceAll("_", " ");
}
