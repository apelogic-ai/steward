const expectedAuthorizations = new Set([
  "Bearer fixture-provider-token",
  "Bearer fixture-service-provider-token",
]);

Bun.serve({
  port: 8082,
  async fetch(request) {
    const authorization = request.headers.get("authorization");
    if (!authorization || !expectedAuthorizations.has(authorization)) {
      return Response.json(
        {
          jsonrpc: "2.0",
          id: null,
          error: { code: -32001, message: "fixture provider credential rejected" },
        },
        { status: 401 },
      );
    }

    const body = (await request.json()) as {
      id?: string | number | null;
      method?: string;
      params?: { name?: string };
    };
    if (body.method === "initialize") {
      return Response.json({
        jsonrpc: "2.0",
        id: body.id ?? null,
        result: {
          protocolVersion: "2025-06-18",
          capabilities: { tools: {} },
          serverInfo: { name: "neutral-github-fixture", version: "1.0.0" },
        },
      });
    }
    if (body.method === "notifications/initialized") {
      return new Response(null, { status: 202 });
    }
    if (body.method === "tools/list") {
      return Response.json({
        jsonrpc: "2.0",
        id: body.id ?? null,
        result: {
          tools: [
            {
              name: "search_repositories",
              description: "Return the neutral fixture repository.",
              inputSchema: { type: "object", additionalProperties: false },
            },
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
              name: "create_issue",
              description: "A write-shaped fixture tool.",
              inputSchema: { type: "object", additionalProperties: false },
            },
          ],
        },
      });
    }
    if (body.method === "tools/call" && body.params?.name === "search_repositories") {
      return Response.json({
        jsonrpc: "2.0",
        id: body.id ?? null,
        result: {
          content: [{ type: "text", text: "example-org/fixture-repository" }],
        },
      });
    }
    if (body.method === "tools/call" && body.params?.name === "get_file_contents") {
      return Response.json({
        jsonrpc: "2.0",
        id: body.id ?? null,
        result: {
          content: [{ type: "text", text: "governed fixture file contents" }],
        },
      });
    }
    return Response.json({
      jsonrpc: "2.0",
      id: body.id ?? null,
      error: { code: -32601, message: "fixture method not found" },
    });
  },
});
