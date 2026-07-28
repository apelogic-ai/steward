import { expect, test } from "bun:test";

import type { Hop1Identity } from "../shared/identity/hop1";
import { encryptSecret } from "../shared/oauth/crypto";
import { GitHubTokenBroker } from "../shared/oauth/github";
import { InMemoryOAuthTokenStore } from "../shared/oauth/memory-store";
import { createGithubMcpProxyHandler } from "../servers/github-mcp/wrapper/src/proxy";

const issuer = "https://issuer.example.test";
const alice: Hop1Identity = {
  profile: "fixture",
  issuer,
  subject: "runtime-alice",
  email: "alice@example.com",
  claims: {},
};
const bob: Hop1Identity = {
  profile: "fixture",
  issuer,
  subject: "runtime-bob",
  email: "bob@example.org",
  claims: {},
};

test("one subject cannot resolve another subject's provider credential", async () => {
  const encryptionKey = Buffer.alloc(32, 7).toString("base64");
  const store = new InMemoryOAuthTokenStore();
  const now = new Date();
  await store.saveAccount({
    provider: "github",
    hop1Issuer: alice.issuer,
    hop1Subject: alice.subject,
    email: alice.email,
    scopesGranted: ["repo"],
    encryptedRefreshToken: encryptSecret("fixture-provider-token", encryptionKey),
    createdAt: now,
    updatedAt: now,
  });
  const broker = new GitHubTokenBroker({
    config: {
      clientId: "",
      clientSecret: "",
      redirectUri: "",
      tokenEncryptionKey: encryptionKey,
    },
    tokenStore: store,
  });
  const upstreamAuthorizations: string[] = [];
  const handler = createGithubMcpProxyHandler({
    upstreamUrl: "https://provider.example.test/mcp",
    authenticate: (token) => Promise.resolve(token === "hop1-alice" ? alice : bob),
    resolveGithubToken: (identity) => broker.getAccessToken(identity, ["repo"]),
    fetch: (request) => {
      upstreamAuthorizations.push(request.headers.get("authorization") ?? "");
      return Promise.resolve(
        Response.json({
          jsonrpc: "2.0",
          id: 1,
          result: {
            content: [{ type: "text", text: "example-org/fixture-repository" }],
          },
        }),
      );
    },
  });

  const aliceResponse = await callTool(handler, "hop1-alice");
  expect(aliceResponse.status).toBe(200);
  expect(await aliceResponse.text()).toContain("example-org/fixture-repository");

  const bobResponse = await callTool(handler, "hop1-bob");
  expect(bobResponse.status).toBe(401);
  expect(await bobResponse.json()).toEqual({
    jsonrpc: "2.0",
    id: null,
    error: {
      code: -32001,
      message: "Unauthorized: GitHub account is not connected",
    },
  });
  expect(upstreamAuthorizations).toEqual(["Bearer fixture-provider-token"]);
});

function callTool(
  handler: (request: Request) => Promise<Response>,
  token: string,
): Promise<Response> {
  return handler(
    new Request("https://gateway.example.test/mcp", {
      method: "POST",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: "tools/call",
        params: {
          name: "search_repositories",
          arguments: {},
        },
      }),
    }),
  );
}
