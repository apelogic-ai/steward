export type PublishedWorkflow = {
  agent: string;
  displayName: string;
  name: string;
  version: number;
};

export const ONBOARDING_WORKFLOW_PATH_KEY = "steward.ui.onboarding.workflow-path";

export function workflowReference(workflow: PublishedWorkflow): string {
  return `${workflow.name}@${workflow.version}`;
}

export function workflowRenderRequest(workflow: PublishedWorkflow): Record<string, string> {
  return {
    workflow: workflowReference(workflow),
  };
}
