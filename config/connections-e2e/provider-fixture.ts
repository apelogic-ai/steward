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
  return typeof value === "string" || typeof value === "number" || value === null ? value : null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
