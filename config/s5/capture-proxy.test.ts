import { afterEach, describe, expect, test } from "bun:test";

const servers: Bun.Server<unknown>[] = [];
const children: ReturnType<typeof Bun.spawn>[] = [];

afterEach(async () => {
  for (const child of children.splice(0)) {
    child.kill();
    await child.exited;
  }
  for (const server of servers.splice(0)) server.stop(true);
});

const fixtureServer = (
  fetch: (request: Request) => Response | Promise<Response>,
): Bun.Server<unknown> => {
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch });
  servers.push(server);
  return server;
};

describe("HOP-1 capture proxy", () => {
  test("preserves the MCP session established by initialize", async () => {
    const inference = fixtureServer(() => new Response("ok"));
    const mint = fixtureServer(() => new Response("ok"));
    const receivedSessionIds: Array<string | null> = [];
    const mcp = fixtureServer(async (request) => {
      receivedSessionIds.push(request.headers.get("mcp-session-id"));
      const body = (await request.json()) as { method?: string };
      if (body.method === "notifications/initialized") {
        return new Response(null, { status: 202 });
      }
      return Response.json(
        { jsonrpc: "2.0", id: 1, result: { protocolVersion: "2025-06-18" } },
        { headers: { "mcp-session-id": "session-test" } },
      );
    });
    const child = Bun.spawn(["bun", "run", "config/s5/capture-proxy.ts"], {
      cwd: import.meta.dir.replace(/\/config\/s5$/, ""),
      env: {
        ...process.env,
        INFERENCE_URL: inference.url.toString(),
        MCP_URL: mcp.url.toString(),
        TOOLS_MINT_URL: mint.url.toString(),
      },
      stdout: "pipe",
      stderr: "pipe",
    });
    children.push(child);

    let ready = false;
    for (let attempt = 0; attempt < 50; attempt += 1) {
      ready = await fetch("http://127.0.0.1:8085/health")
        .then((response) => response.ok)
        .catch(() => false);
      if (ready) break;
      await Bun.sleep(20);
    }
    expect(ready).toBe(true);

    const response = await fetch("http://127.0.0.1:8085/mcp", {
      method: "POST",
      headers: {
        authorization: "Bearer test-placeholder",
        "content-type": "application/json",
        "mcp-protocol-version": "2025-06-18",
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize" }),
    });

    expect(response.status).toBe(200);
    expect(response.headers.get("mcp-session-id")).toBe("session-test");
    expect(response.headers.get("connection")).toBe("close");

    const initialized = await fetch("http://127.0.0.1:8085/mcp", {
      method: "POST",
      headers: {
        authorization: "Bearer test-placeholder",
        "content-type": "application/json",
        "mcp-protocol-version": "2025-06-18",
        "mcp-session-id": "session-test",
      },
      body: JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }),
    });

    expect(initialized.status).toBe(202);
    expect(initialized.headers.get("content-type")).toBeNull();
    expect(receivedSessionIds).toEqual([null, "session-test"]);
  });
});
