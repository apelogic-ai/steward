export function isIssue414RunFailure({
  knownIssue,
  statedSingleRunLimit,
  phase,
  finalized,
  failureReason,
  atDeadline,
}) {
  const reasonIsEmpty = failureReason == null || failureReason.trim() === "";
  if (knownIssue !== "@known-failing-414" || statedSingleRunLimit !== "0" || !reasonIsEmpty) return false;
  if (finalized) return phase !== "succeeded";
  return atDeadline && phase === "running";
}
