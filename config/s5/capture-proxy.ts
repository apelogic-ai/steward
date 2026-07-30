const required = (name: string): string => {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is required`);
  }
  return value;
};

const inferenceUrl = required("INFERENCE_URL");
const mcpUrl = required("MCP_URL");
const toolsMintUrl = required("TOOLS_MINT_URL");

type CapturedRequest = {
  authorization: string;
  body: string;
  contentType: string;
  protocolVersion?: string;
};

let inferenceRequest: CapturedRequest | undefined;
let toolRequest: CapturedRequest | undefined;
let tokenGrant: { body: string; contentType: string } | undefined;

const forwardedResponse = async (response: Response): Promise<Response> =>
  new Response(await response.arrayBuffer(), {
    status: response.status,
    headers: {
      "cache-control": "no-store",
      "content-type":
        response.headers.get("content-type") ?? "application/octet-stream",
    },
  });

const captureBearerRequest = async (
  request: Request,
  target: string,
  remember: (captured: CapturedRequest) => void,
): Promise<Response> => {
  const authorization = request.headers.get("authorization");
  if (!authorization) {
    return Response.json({ error: "missing bearer credential" }, { status: 401 });
  }
  const body = await request.text();
  const captured: CapturedRequest = {
    authorization,
    body,
    contentType:
      request.headers.get("content-type") ?? "application/octet-stream",
    protocolVersion:
      request.headers.get("mcp-protocol-version") ?? undefined,
  };
  remember(captured);
  return forwardedResponse(
    await fetch(target, {
      method: "POST",
      headers: requestHeaders(captured),
      body,
    }),
  );
};

const requestHeaders = (captured: CapturedRequest): Headers => {
  const headers = new Headers({
    authorization: captured.authorization,
    "content-type": captured.contentType,
  });
  if (captured.protocolVersion) {
    headers.set("mcp-protocol-version", captured.protocolVersion);
  }
  return headers;
};

const replayBearer = async (
  captured: CapturedRequest | undefined,
  target: string,
): Promise<Response> => {
  if (!captured) {
    return Response.json({ error: "no credential was captured" }, { status: 409 });
  }
  const upstream = await fetch(target, {
    method: "POST",
    headers: requestHeaders(captured),
    body: captured.body,
  });
  return Response.json(
    { upstreamStatus: upstream.status },
    { headers: { "cache-control": "no-store" } },
  );
};

Bun.serve({
  hostname: "0.0.0.0",
  port: 8085,
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === "/health") {
      return new Response("ok");
    }
    if (path === "/token-tools" && request.method === "POST") {
      const body = await request.text();
      const contentType =
        request.headers.get("content-type") ??
        "application/x-www-form-urlencoded";
      tokenGrant = { body, contentType };
      return forwardedResponse(
        await fetch(toolsMintUrl, {
          method: "POST",
          headers: { "content-type": contentType },
          body,
        }),
      );
    }
    if (path === "/mcp" && request.method === "POST") {
      return captureBearerRequest(request, mcpUrl, (captured) => {
        toolRequest = captured;
      });
    }
    if (path === "/inference" && request.method === "POST") {
      return captureBearerRequest(request, inferenceUrl, (captured) => {
        inferenceRequest = captured;
      });
    }
    if (path === "/replay-token" && request.method === "POST") {
      if (!tokenGrant) {
        return Response.json(
          { error: "no token grant was captured" },
          { status: 409 },
        );
      }
      const upstream = await fetch(toolsMintUrl, {
        method: "POST",
        headers: { "content-type": tokenGrant.contentType },
        body: tokenGrant.body,
      });
      return Response.json(
        { upstreamStatus: upstream.status },
        { headers: { "cache-control": "no-store" } },
      );
    }
    if (path === "/replay-tool" && request.method === "POST") {
      return replayBearer(toolRequest, mcpUrl);
    }
    if (path === "/replay-inference" && request.method === "POST") {
      return replayBearer(inferenceRequest, inferenceUrl);
    }
    return Response.json({ error: "not found" }, { status: 404 });
  },
});
