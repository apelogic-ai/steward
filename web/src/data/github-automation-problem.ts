import type { GithubAutomationErrorResponse } from "@/api-client";

export const genericBundleFailure = "Steward could not render the exact tested package and workflow.";

const unpublishableReasons: Record<string, string> = {
  evidence_unavailable: "The test run recorded no package evidence that Steward can publish. Run the test again.",
  source_not_inline: "This test run used a package that already lives in a repository or the Workflow registry. Run the inline test from step 3 to publish it here.",
  package_files_invalid: "The test run's recorded package files are incomplete or invalid, so Steward cannot publish them. Run the test again.",
  closure_mismatch: "The test run's recorded package files no longer match its tested digest, so Steward will not publish them. Run the test again.",
  package_shape_unsupported: "Governed publication does not support this tested package's files. It publishes a Task definition under .steward/tasks/, or a root task-definition.json with the prompt.md beside it.",
  repository_root_conflict: "The repository's default branch already has a different task-definition.json or prompt.md at its root. Steward will not overwrite it; move or remove that file, then publish again.",
  envelope_unavailable: "The test run recorded no complete envelope selection, so Steward cannot generate its workflow. Run the test again.",
  workflow_unavailable: "Steward could not generate the caller workflow for this package. Ask an administrator to check the configured steward-run release.",
};

/** The specific explanation for a bounded `tested_package_unpublishable` reason, else the fallback. */
export function automationProblemMessage(problem: GithubAutomationErrorResponse | undefined, fallback: string): string {
  if (problem?.error === "tested_package_unpublishable" && problem.reason) {
    return unpublishableReasons[problem.reason] ?? fallback;
  }
  return fallback;
}

export function bundleFailureMessage(problem: GithubAutomationErrorResponse | undefined): string {
  if (problem?.error === "steward_run_release_unsupported") {
    return "The reviewed steward-run release is below 0.8.0. Copy the package files manually; workflow generation and detection require steward-run 0.8.0 or later.";
  }
  return automationProblemMessage(problem, genericBundleFailure);
}
