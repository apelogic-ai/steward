export type MutationFailureState = "conflict" | "rejected" | "forbidden" | "unavailable" | "error";

export function classifyMutationFailure(status: number | undefined): MutationFailureState {
  if (status === 401 || status === 403) return "forbidden";
  if (status === 409) return "conflict";
  if (status === 400 || status === 422) return "rejected";
  if (status === 502 || status === 503) return "unavailable";
  return "error";
}
