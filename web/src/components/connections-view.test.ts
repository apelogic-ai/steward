import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

const source = readFileSync(new URL("./connections-view.tsx", import.meta.url), "utf8");

describe("governed provider connection controls", () => {
  test("warns that disconnect affects every present and future runtime", () => {
    expect(source).toContain("every current and future runtime");
    expect(source).toContain("using your identity");
  });

  test("distinguishes an outstanding OAuth flow from an ordinary conflict", () => {
    expect(source).toContain("Finish or wait for the pending GitHub authorization");
  });

  test("explains that staged orchestration deliberately disables connection mutations", () => {
    expect(source).toContain("orchestration-not-active");
    expect(source).toContain("Connections are disabled until task orchestration is active (stage 2).");
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
    expect(source).toContain("MAX_CONNECTION_POLL_MS");
    expect(source).toContain('setAction("poll-expired")');
    expect(source).toContain("if (controller.signal.aborted) return;");
    expect(source).toContain("Authorization did not become ready in time. Retry the connection");
  });

  test("shows the bounded terminal connection failure category", () => {
    expect(source).toContain("GitHub authorization failed");
    expect(source).toContain("failure.error");
    expect(source).toContain("failure.detail");
    expect(source).toContain("runtime_authentication_failed");
    expect(source).toContain("verify runtime credential injection");
    expect(source).toContain("token_grant_failed");
    expect(source).toContain("inspect MCP-GW token grants");
    expect(source).toContain("runtime_create_failed");
    expect(source).toContain("inspect Steward runtime admission and controller events");
    expect(source).toContain("runtime_start_failed");
    expect(source).toContain("inspect the AgentRuntime and OpenShell sandbox status");
    expect(source).toContain("connection_deadline_exceeded");
    expect(source).toContain("inspect runtime health");
  });

  test("clears a stale authorization failure before disconnecting", () => {
    expect(source).toMatch(/async function disconnect\(\) \{[\s\S]*?setStartFailure\(null\);[\s\S]*?setAction\("working"\);/);
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
});
