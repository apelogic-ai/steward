#!/usr/bin/env bun

const port = Number(process.env.PORT ?? "8090");
const fixtureEmail = process.env.FIXTURE_EMAIL ?? "alice@example.com";
const githubScopes = process.env.FIXTURE_GITHUB_SCOPES ?? "repo";
const providerToken = "obviously-fake-provider-token";

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
              {
                name: "get_file_contents",
                description: "Return neutral fixture file contents.",
                inputSchema: {
                  type: "object",
                  properties: {
                    owner: { type: "string" },
                    repo: { type: "string" },
                    path: { type: "string" },
                  },
                  required: ["owner", "repo", "path"],
                  additionalProperties: false,
                },
              },
              {
                name: "actions_run_trigger",
                description: "Exercise the governed GitHub Actions mutation boundary.",
                annotations: {
                  readOnlyHint: false,
                  destructiveHint: true,
                  idempotentHint: false,
                },
                inputSchema: {
                  type: "object",
                  properties: {
                    method: { type: "string" },
                    owner: { type: "string" },
                    repo: { type: "string" },
                    run_id: { type: "number" },
                  },
                  required: ["method", "owner", "repo"],
                  additionalProperties: false,
                },
              },
            ],
          });
        }
        if (
          payload.method === "tools/call" &&
          isRecord(payload.params) &&
          payload.params.name === "get_file_contents"
        ) {
          return rpcResult(id, {
            content: [{ type: "text", text: "governed fixture file contents" }],
          });
        }
        if (
          payload.method === "tools/call" &&
          isRecord(payload.params) &&
          payload.params.name === "actions_run_trigger" &&
          isRecord(payload.params.arguments)
        ) {
          const args = payload.params.arguments;
          const exactKeys = Object.keys(args).sort().join(",") === "method,owner,repo,run_id";
          if (
            exactKeys &&
            args.method === "rerun_workflow_run" &&
            args.owner === "example-org" &&
            args.repo === "example-repo" &&
            args.run_id === 12345
          ) {
            return rpcResult(id, {
              content: [{ type: "text", text: "provider detail that Steward must discard" }],
              structuredContent: { accepted: true },
              isError: false,
            });
          }
          return rpcResult(id, {
            content: [{ type: "text", text: "invalid rerun fixture request" }],
            structuredContent: { error: "invalid_rerun_fixture_request" },
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

function jsonRpcId(value: unknown): string | number | null {
  if (typeof value === "string" || typeof value === "number") return value;
  return null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
