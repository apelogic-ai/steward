#!/usr/bin/env bun

const port = Number(process.env.PORT ?? "8090");
const fixtureEmail = process.env.FIXTURE_EMAIL ?? "alice@example.com";
const githubScopes = process.env.FIXTURE_GITHUB_SCOPES ?? "repo";
const providerToken = "obviously-fake-provider-token";
const governedWorkflow = [
  "on:",
  "  workflow_dispatch:",
  "    inputs:",
  "      message:",
  "jobs:",
  "  governed:",
  "    uses: example-org/steward-run/.github/workflows/steward-task.yml@0123456789012345678901234567890123456789",
  "",
].join("\n");
let publicationBranch = "";
let workflowDispatched = false;

Bun.serve({
  port,
  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === "/health") {
      return Response.json({ ok: true });
    }
    if (request.method === "POST" && url.pathname === "/github/token") {
      return Response.json({
        access_token: providerToken,
        scope: githubScopes,
        token_type: "bearer",
      });
    }
    if (request.method === "GET" && url.pathname === "/github/emails") {
      return withProviderToken(request, () =>
        Response.json([{ email: fixtureEmail, primary: true, verified: true }]),
      );
    }
    if (request.method === "GET" && url.pathname === "/github/user") {
      return withProviderToken(request, () => Response.json({ id: 12345, login: "alice" }));
    }
    if (request.method === "DELETE" && url.pathname === "/github/revoke") {
      const body = (await request.json()) as { access_token?: string };
      return body.access_token === providerToken
        ? new Response(null, { status: 204 })
        : Response.json({ error: "provider credential rejected" }, { status: 401 });
    }
    if (request.method === "POST" && url.pathname === "/github-mcp") {
      return withProviderToken(request, async () => {
        const payload = (await request.json()) as Record<string, unknown>;
        const id = jsonRpcId(payload.id);
        if (payload.method === "initialize") {
          return rpcResult(id, {
            protocolVersion: "2025-06-18",
            capabilities: { tools: {} },
            serverInfo: { name: "neutral-github-fixture", version: "1.0.0" },
          });
        }
        if (payload.method === "notifications/initialized") {
          return new Response(null, { status: 202 });
        }
        if (payload.method === "tools/list") {
          return rpcResult(id, {
            tools: [
              readTool("get_me"),
              readTool("search_repositories"),
              readTool("get_file_contents"),
              readTool("actions_list"),
              writeTool("actions_run_trigger"),
              writeTool("create_branch"),
              writeTool("push_files"),
              writeTool("create_pull_request"),
            ],
          });
        }
        if (payload.method === "tools/call" && isRecord(payload.params)) {
          const name = payload.params.name;
          const args = isRecord(payload.params.arguments) ? payload.params.arguments : {};
          if (name === "get_me") {
            return toolResult(id, { login: "alice", id: 1000001 });
          }
          if (name === "search_repositories") {
            return toolResult(id, {
              total_count: 1,
              incomplete_results: false,
              items: [
                {
                  id: 1296269,
                  name: "example-repo",
                  full_name: "example-org/example-repo",
                  owner: { login: "example-org", id: 1000002, type: "Organization" },
                  html_url: "https://github.com/example-org/example-repo",
                  private: false,
                  default_branch: "main",
                },
              ],
            });
          }
          if (name === "get_file_contents") {
            const content =
              args.path === ".github/workflows/steward-browser-task.yml"
                ? governedWorkflow
                : "governed fixture file contents";
            return toolResult(id, {
              content,
              sha: "0123456789abcdef0123456789abcdef01234567",
            });
          }
          if (name === "create_branch") {
            publicationBranch = typeof args.branch === "string" ? args.branch : "";
            return toolResult(id, {
              ref: `refs/heads/${publicationBranch}`,
              object: { sha: "1111111111111111111111111111111111111111" },
            });
          }
          if (name === "push_files") {
            return toolResult(id, {
              commit: { sha: "2222222222222222222222222222222222222222" },
            });
          }
          if (name === "create_pull_request") {
            const branch = typeof args.head === "string" ? args.head : publicationBranch;
            return toolResult(id, {
              number: 17,
              html_url: "https://github.com/example-org/example-repo/pull/17",
              state: "open",
              head: { ref: branch },
              base: { ref: "main" },
            });
          }
          if (name === "actions_list") {
            return toolResult(
              id,
              workflowDispatched
                ? {
                    workflow_runs: [
                      {
                        id: 101,
                        event: "workflow_dispatch",
                        html_url:
                          "https://github.com/example-org/example-repo/actions/runs/101",
                      },
                    ],
                  }
                : { workflow_runs: [] },
            );
          }
          if (name === "actions_run_trigger" && args.method === "run_workflow") {
            workflowDispatched = true;
            return toolResult(id, {});
          }
          if (
            name === "actions_run_trigger" &&
            args.method === "rerun_workflow_run" &&
            args.owner === "example-org" &&
            args.repo === "example-repo" &&
            args.run_id === 12345
          ) {
            return toolResult(id, { accepted: true }, "provider detail that Steward must discard");
          }
          return rpcResult(id, {
            content: [{ type: "text", text: "invalid governed fixture request" }],
            structuredContent: { error: "invalid_governed_fixture_request" },
            isError: true,
          });
        }
        return Response.json(
          { jsonrpc: "2.0", id, error: { code: -32601, message: "Method not found" } },
          { status: 404 },
        );
      });
    }
    return new Response("not found", { status: 404 });
  },
});

async function withProviderToken(
  request: Request,
  handler: () => Response | Promise<Response>,
): Promise<Response> {
  if (request.headers.get("authorization") !== `Bearer ${providerToken}`) {
    return Response.json({ error: "provider credential rejected" }, { status: 401 });
  }
  return handler();
}

function rpcResult(id: string | number | null, result: unknown): Response {
  return Response.json({ jsonrpc: "2.0", id, result });
}

function toolResult(
  id: string | number | null,
  structuredContent: Record<string, unknown>,
  text = "fixture operation completed",
): Response {
  return rpcResult(id, {
    content: [{ type: "text", text }],
    structuredContent,
    isError: false,
  });
}

function readTool(name: string): Record<string, unknown> {
  return {
    name,
    description: `Read neutral ${name} fixture data.`,
    annotations: { readOnlyHint: true, destructiveHint: false, idempotentHint: true },
    inputSchema: { type: "object", additionalProperties: true },
  };
}

function writeTool(name: string): Record<string, unknown> {
  return {
    name,
    description: `Exercise the governed ${name} mutation boundary.`,
    annotations: { readOnlyHint: false, destructiveHint: true, idempotentHint: false },
    inputSchema: { type: "object", additionalProperties: true },
  };
}

function jsonRpcId(value: unknown): string | number | null {
  if (typeof value === "string" || typeof value === "number") return value;
  return null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
