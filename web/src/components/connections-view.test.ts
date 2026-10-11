import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

const source = readFileSync(new URL("./connections-view.tsx", import.meta.url), "utf8");

describe("governed provider connection controls", () => {
  test("uses plain connection failure messages", () => {
    expect(source).toContain("Connections are unavailable. Try again.");
    expect(source).toContain("The connection action could not be completed.");
    expect(source).not.toContain("authoritative connection service");
    expect(source).not.toContain("server-owned connection action");
  });

  test("warns that disconnect affects every present and future run", () => {
    expect(source).toContain("every current and future run");
    expect(source).toContain("using your account");
  });

  test("distinguishes an outstanding OAuth flow from an ordinary conflict", () => {
    expect(source).toContain("Finish or wait for the pending GitHub authorization");
  });

  test("explains that connections stay disabled until task runs are enabled", () => {
    expect(source).toContain("orchestration-not-active");
    expect(source).toContain("Connections are disabled until task runs are enabled for this installation.");
  });

  test("polls an accepted connection start and aborts it when the view unmounts", () => {
    expect(source).toContain("getProviderConnectionStartOperation");
    expect(source).toContain('result.response?.status !== 202');
    expect(source).toContain('operation.data?.state !== "pending"');
    expect(source).toContain('operation.data?.state === "succeeded"');
    expect(source).toContain("startController.current?.abort()");
    expect(source).toContain("signal: controller.signal");
  });

  test("bounds a perpetually pending operation by the server deadline", () => {
    expect(source).toContain("result.data.pollDeadlineAt");
    expect(source).toContain("boundedConnectionPollDeadline");
    expect(source).toContain('setAction("poll-expired")');
    expect(source).toContain("if (controller.signal.aborted) return;");
    expect(source).toContain("The connection operation did not finish in time. Retry it");
  });

  test("shows the bounded terminal connection failure category", () => {
    expect(source).toContain("GitHub authorization couldn't be completed");
    expect(source).toContain("<summary className=\"cursor-pointer\">Details</summary>");
    expect(source).toContain("{startFailure.error}");
    expect(source).toContain("{startFailure.code}");
    expect(source).toContain("runtime_authentication_failed");
    expect(source).toContain("Re-authorize GitHub");
    expect(source).toContain("proxy_policy_denied");
    expect(source).toContain("current connection settings");
    expect(source).toContain("provider_authorization_failed");
    expect(source).toContain("review its access");
    expect(source).toContain("token_grant_failed");
    expect(source).toContain("GitHub access couldn't be prepared");
    expect(source).toContain("provider_response_invalid");
    expect(source).toContain("GitHub returned an unexpected response");
    expect(source).toContain("gateway_transport_failed");
    expect(source).toContain("Steward couldn't reach GitHub");
    expect(source).toContain("gateway_status_invalid");
    expect(source).toContain("installed connection services");
    expect(source).toContain("gateway_body_unavailable");
    expect(source).toContain("gateway_unavailable");
    expect(source).toContain("gateway_session_failed");
    expect(source).toContain("GitHub connection couldn't be started");
    expect(source).toContain("runtime_create_failed");
    expect(source).toContain("temporary GitHub connection couldn't be created");
    expect(source).toContain("runtime_start_failed");
    expect(source).toContain("temporary GitHub connection couldn't start");
    expect(source).toContain("connection_deadline_exceeded");
    expect(source).toContain("did not become ready in time");
    expect(source).toContain("bridge_result_too_large");
    expect(source).toContain("more data than this operation can process");
    expect(source).toContain("oauth_redirect_target_not_allowed");
    expect(source).toContain("connection's return address");
    expect(source).toContain("code: operation.data.code");
    expect(source).not.toContain("verify runtime credential injection");
    expect(source).not.toContain("inspect MCP-GW token grants");
    expect(source).not.toContain("githubWrapper.oauth.redirectAfterAllowedOrigins");
  });

  test("clears a stale authorization failure before disconnecting", () => {
    expect(source).toMatch(/async function disconnect\(\) \{[\s\S]*?setStartFailure\(null\);[\s\S]*?setAction\("working"\);/);
  });

  test("renders the structured failure returned by disconnect", () => {
    expect(source).toMatch(/async function disconnect\(\) \{[\s\S]*?setStartFailure\(connectionOperationError\(result\.error\)\);/);
    expect(source).toMatch(/async function disconnect\(\) \{[\s\S]*?result\.response\?\.status !== 202[\s\S]*?getProviderConnectionStartOperation/);
    expect(source).toMatch(/operation\.data\?\.state === "succeeded" && !operation\.data\.authorizationUrl/);
  });

  test("shows a reauthorization action when a reported credential deadline approaches", () => {
    expect(source).toContain('connectionHealth(status)');
    expect(source).toContain('Re-authorize GitHub');
    expect(source).toContain('renewalCredentialExpiresAt');
  });

  test("keeps the provider card and recovery action available without status metadata", () => {
    expect(source).toContain('<ProviderConnection metadataState={state.status}');
    expect(source).toContain('Authorize / re-authorize GitHub');
    expect(source).toContain('Status unavailable');
  });

  test("shows the stable GitHub account identity and Actions link state", () => {
    expect(source).toContain('GitHub ID ${status.accountId}');
    expect(source).toContain('GitHub Actions runs as you: linked.');
    expect(source).toContain('status?.githubActionsIdentityLinked === false');
  });

  test("separates tool connections from deployment-mode-aware inference custody", () => {
    expect(source).toContain("Tools / MCP servers");
    expect(source).toContain("<h2 className=\"text-lg font-semibold\">Models</h2>");
    expect(source).toContain("getInferenceConnection");
    expect(source).toContain('value?.mode === "stock"');
    expect(source).toContain("API keys are managed by your Steward administrator");
    expect(source).toContain('type="password"');
    expect(source).toContain("Add API key");
    expect(source).toContain("Replace API key");
    expect(source).toContain("Remove API key…");
    expect(source).toContain("Remove stored API key…");
    expect(source).toContain("A saved key from");
    expect(source).toContain("Runs started here or from GitHub Actions use this key");
    expect(source).toContain("It does not revoke or delete the key at the model provider.");
  });

  test("never renders the submitted inference credential", () => {
    expect(source).toContain("setKey(\"\")");
    expect(source).not.toContain("credential.apiKey");
  });
});
