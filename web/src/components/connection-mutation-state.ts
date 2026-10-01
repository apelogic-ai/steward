import { classifyMutationFailure, type MutationFailureState } from "@/data/mutation-state";

export type ConnectionMutationState = "orchestration-not-active" | "oauth-pending" | MutationFailureState;

export function classifyConnectionMutationFailure(
  status: number | undefined,
  error: unknown,
): ConnectionMutationState {
  const code = typeof error === "object" && error !== null && "error" in error
    ? (error as { error?: unknown }).error
    : undefined;
  if (status === 503 && code === "connections.orchestration_not_active") {
    return "orchestration-not-active";
  }
  if (status === 409 && code === "oauth_flow_pending") return "oauth-pending";
  return classifyMutationFailure(status);
}
