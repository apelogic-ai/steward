const expectedAuthorization = "Bearer fixture-provider-token";

Bun.serve({
  port: 8082,
  async fetch(request) {
    if (request.headers.get("authorization") !== expectedAuthorization) {
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
    return Response.json({
      jsonrpc: "2.0",
      id: body.id ?? null,
      error: { code: -32601, message: "fixture method not found" },
    });
  },
});
