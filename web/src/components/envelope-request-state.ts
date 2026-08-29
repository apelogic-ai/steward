import type { ConnectionReadiness } from "@/api-client";

export function githubConnectionBlocksSelectedTools(
  selectedToolCount: number,
  readiness: ConnectionReadiness,
) {
  return selectedToolCount > 0 && readiness !== "connected";
}
