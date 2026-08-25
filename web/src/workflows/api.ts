import type { PublishedWorkflow } from "./contracts";
import {
  getAdminWorkflowVersion,
  listAdminWorkflows as requestAdminWorkflows,
  listPublishedWorkflows as requestPublishedWorkflows,
  publishAdminWorkflow,
  publishAdminWorkflowVersion,
  renderGithubActionsForEnvelope,
  type PublishedWorkflowsResponse,
  type WorkflowListResponse,
  type WorkflowRevisionResponse,
  type WorkflowRevisionView,
} from "@/api-client";

export type WorkflowRevision = WorkflowRevisionView;
export type PublishedWorkflowListResponse = PublishedWorkflowsResponse;
export type { WorkflowListResponse, WorkflowRevisionResponse };

export function listAdminWorkflows() {
  return requestAdminWorkflows({ cache: "no-store", credentials: "same-origin" });
}

export function listPublishedWorkflows(): Promise<{
  data?: PublishedWorkflowsResponse;
  response?: Response;
}> {
  return requestPublishedWorkflows({ cache: "no-store", credentials: "same-origin" });
}

export function getAdminWorkflow(name: string, version: number) {
  return getAdminWorkflowVersion({
    cache: "no-store",
    credentials: "same-origin",
    path: { name, version },
  });
}

type WorkflowContent = {
  agent: string;
  displayName: string;
  prompt: string;
};

export function publishWorkflow(
  csrf: string,
  content: WorkflowContent & { name: string },
){
  return publishAdminWorkflow({
    body: content,
    credentials: "same-origin",
    headers: { "X-Steward-CSRF": csrf },
  });
}

export function publishWorkflowVersion(
  csrf: string,
  name: string,
  content: WorkflowContent,
){
  return publishAdminWorkflowVersion({
    body: content,
    credentials: "same-origin",
    headers: { "X-Steward-CSRF": csrf },
    path: { name },
  });
}

export function renderWorkflowForEnvelope(
  csrf: string,
  requestId: string,
  workflow: PublishedWorkflow,
){
  return renderGithubActionsForEnvelope({
    body: { workflow: `${workflow.name}@${workflow.version}` },
    credentials: "same-origin",
    headers: { "X-Steward-CSRF": csrf },
    path: { request_id: requestId },
  });
}
