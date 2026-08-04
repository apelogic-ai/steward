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
const aliceRuntimeUid = "runtime-alice";
const bobRuntimeUid = "runtime-bob";
const carolRuntimeUid = "runtime-carol";
const serviceRuntimeUid = "runtime-service";
const alice: Hop1Identity = {
  profile: "fixture",
  issuer,
  subject: "alice@example.com",
  email: "alice@example.com",
  claims: {
    steward: {
      acting_as: "user",
      runtime_uid: aliceRuntimeUid,
      tools: [aliceTool],
      version: 2,
    },
  },
};
const bob: Hop1Identity = {
  profile: "fixture",
  issuer,
  subject: "bob@example.org",
  email: "bob@example.org",
  claims: {
    steward: {
      acting_as: "user",
      runtime_uid: bobRuntimeUid,
      tools: [aliceTool],
      version: 2,
    },
  },
};
const carol: Hop1Identity = {
  profile: "fixture",
  issuer,
  subject: "carol@example.com",
  email: "carol@example.com",
  claims: {
    steward: {
      acting_as: "user",
      runtime_uid: carolRuntimeUid,
      tools: [],
      version: 2,
    },
  },
};
const scheduledScanner: Hop1Identity = {
  profile: "fixture",
  issuer,
  subject: "service:scheduled-scanner",
  email: "service:scheduled-scanner",
  claims: {
    steward: {
      acting_as: "service",
      service: "scheduled-scanner",
      runtime_uid: serviceRuntimeUid,
      tools: [aliceTool],
      version: 2,
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
  await store.saveAccount({
    provider: "github",
    hop1Issuer: scheduledScanner.issuer,
    hop1Subject: scheduledScanner.subject,
    email: scheduledScanner.email,
    scopesGranted: ["repo"],
    encryptedRefreshToken: encryptSecret(
      "fixture-service-provider-token",
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
      Promise.resolve(
        token === "hop1-alice"
          ? alice
          : token === "hop1-bob"
            ? bob
            : token === "hop1-service"
              ? scheduledScanner
              : carol,
      ),
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
  expect(bobResponse.status).toBe(401);
  expect(await bobResponse.json()).toEqual({
    jsonrpc: "2.0",
    id: null,
    error: {
      code: -32001,
      message: "Unauthorized: GitHub account is not connected",
    },
  });

  const carolResponse = await callTool(handler, "hop1-carol");
  expect(carolResponse.status).toBe(200);
  expect(await carolResponse.json()).toEqual({
    jsonrpc: "2.0",
    id: 1,
    error: {
      code: -32003,
      message:
        "Policy denied search_repositories: verified tool authority missing",
    },
  });
  const serviceResponse = await callTool(handler, "hop1-service");
  expect(serviceResponse.status).toBe(200);
  expect(await serviceResponse.text()).toContain(
    "example-org/fixture-repository",
  );

  expect(policyInputs).toHaveLength(4);
  expect(credentialResolutions).toEqual([
    alice.subject,
    bob.subject,
    scheduledScanner.subject,
  ]);
  expect(upstreamAuthorizations).toEqual([
    "Bearer fixture-provider-token",
    "Bearer fixture-service-provider-token",
  ]);
});

function hasVerifiedToolAuthority(input: ToolPolicyInput): boolean {
  const expectedRuntimeUid =
    input.principal === alice.email
      ? aliceRuntimeUid
      : input.principal === bob.email
        ? bobRuntimeUid
        : input.principal === carol.email
          ? carolRuntimeUid
          : input.principal === scheduledScanner.subject
            ? serviceRuntimeUid
          : undefined;
  const tokenClaims = (
    input as ToolPolicyInput & {
      tokenClaims?: Record<string, unknown>;
    }
  ).tokenClaims;
  const steward = tokenClaims?.steward;
  if (
    !tokenClaims ||
    tokenClaims.sub !== input.principal ||
    !expectedRuntimeUid ||
    typeof steward !== "object" ||
    steward === null ||
    Array.isArray(steward)
  ) {
    return false;
  }
  const claims = steward as Record<string, unknown>;
  const tools = claims.tools;
  const principalClaimsMatch =
    (claims.acting_as === "user" &&
      claims.version === 2 &&
      tokenClaims.email === input.principal) ||
    (claims.acting_as === "service" &&
      claims.version === 2 &&
      claims.service === "scheduled-scanner" &&
      input.principal === "service:scheduled-scanner");
  return (
    principalClaimsMatch &&
    claims.runtime_uid === expectedRuntimeUid &&
    Array.isArray(tools) &&
    tools.some(
      (tool) =>
        typeof tool === "object" &&
        tool !== null &&
        !Array.isArray(tool) &&
        (tool as Record<string, unknown>).provider === input.service &&
        (tool as Record<string, unknown>).resource === input.tool &&
        (tool as Record<string, unknown>).action === input.actionClass,
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
