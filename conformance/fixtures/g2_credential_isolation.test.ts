import { expect, test } from "bun:test";

import type { Hop1Identity } from "../shared/identity/hop1";
import { encryptSecret } from "../shared/oauth/crypto";
import { GitHubTokenBroker } from "../shared/oauth/github";
import { InMemoryOAuthTokenStore } from "../shared/oauth/memory-store";
import type { ToolPolicyInput } from "../shared/policy/policy";
import { createGithubMcpProxyHandler } from "../servers/github-mcp/wrapper/src/proxy";

const issuer = "https://issuer.example.test";
const aliceTool = {
  provider: "github",
  resource: "search_repositories",
  action: "read",
};
const alice: Hop1Identity = {
  profile: "fixture",
  issuer,
  subject: "runtime-alice",
  email: "alice@example.com",
  claims: {
    steward: {
      acting_as: "user",
      runtime_uid: "runtime-alice",
      tools: [aliceTool],
      version: 1,
    },
  },
};
const bob: Hop1Identity = {
  profile: "fixture",
  issuer,
  subject: "runtime-bob",
  email: "bob@example.org",
  claims: {
    steward: {
      acting_as: "user",
      runtime_uid: "runtime-bob",
      tools: [],
      version: 1,
    },
  },
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
    encryptedRefreshToken: encryptSecret(
      "fixture-provider-token",
      encryptionKey,
    ),
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
  const credentialResolutions: string[] = [];
  const policyInputs: ToolPolicyInput[] = [];
  const upstreamAuthorizations: string[] = [];
  const handler = createGithubMcpProxyHandler({
    upstreamUrl: "https://provider.example.test/mcp",
    authenticate: (token) =>
      Promise.resolve(token === "hop1-alice" ? alice : bob),
    resolveGithubToken: (identity) => {
      credentialResolutions.push(identity.subject);
      return broker.getAccessToken(identity, ["repo"]);
    },
    policy: {
      decide: (input) => {
        policyInputs.push(input);
        return Promise.resolve(
          hasVerifiedToolAuthority(input)
            ? { kind: "allow" }
            : {
                kind: "deny",
                reason: "verified tool authority missing",
              },
        );
      },
    },
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
  expect(await aliceResponse.text()).toContain(
    "example-org/fixture-repository",
  );

  const bobResponse = await callTool(handler, "hop1-bob");
  expect(bobResponse.status).toBe(200);
  expect(await bobResponse.json()).toEqual({
    jsonrpc: "2.0",
    id: 1,
    error: {
      code: -32003,
      message:
        "Policy denied search_repositories: verified tool authority missing",
    },
  });
  expect(policyInputs).toHaveLength(2);
  expect(credentialResolutions).toEqual(["runtime-alice"]);
  expect(upstreamAuthorizations).toEqual(["Bearer fixture-provider-token"]);
});

function hasVerifiedToolAuthority(input: ToolPolicyInput): boolean {
  const expectedSubject =
    input.principal === alice.email
      ? alice.subject
      : input.principal === bob.email
        ? bob.subject
        : "";
  const tokenClaims = (
    input as ToolPolicyInput & {
      tokenClaims?: Record<string, unknown>;
    }
  ).tokenClaims;
  const steward = tokenClaims?.steward;
  if (
    !tokenClaims ||
    tokenClaims.email !== input.principal ||
    tokenClaims.sub !== expectedSubject ||
    typeof steward !== "object" ||
    steward === null ||
    Array.isArray(steward)
  ) {
    return false;
  }
  const claims = steward as Record<string, unknown>;
  const tools = claims.tools;
  return (
    claims.runtime_uid === expectedSubject &&
    Array.isArray(tools) &&
    tools.some(
      (tool) =>
        typeof tool === "object" &&
        tool !== null &&
        !Array.isArray(tool) &&
        (tool as Record<string, unknown>).provider === aliceTool.provider &&
        (tool as Record<string, unknown>).resource === input.tool &&
        (tool as Record<string, unknown>).action === aliceTool.action,
    )
  );
}

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
