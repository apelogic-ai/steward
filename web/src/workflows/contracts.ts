export type PublishedWorkflow = {
  agent: string;
  displayName: string;
  name: string;
  version: number;
};

export function workflowReference(workflow: PublishedWorkflow): string {
  return `${workflow.name}@${workflow.version}`;
}

export function workflowRenderRequest(workflow: PublishedWorkflow): Record<string, string> {
  return {
    workflow: workflowReference(workflow),
  };
}
