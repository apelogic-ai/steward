import { describe, expect, test } from "bun:test";

import {
  ExchangeRefusedError,
  deleteCallerTask,
  downloadCallerOutput,
  exactHttpUrl,
  exchangeTaskToken,
  jiraPayload,
  jiraSignature,
  outputArchiveContains,
  requireLiveJiraAuthorization,
  runLiveJiraHelper,
  sendJiraDelivery,
  sourceAssertionFromHelper,
  startCallerTask,
  unboundConfiguration,
  waitForCallerTask,
} from "./caller-http.mjs";

function jsonResponse(body, status = 200, headers = {}) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "cache-control": "private, no-store", "content-type": "application/json", ...headers },
  });
}

function config() {
  return {
    apiOrigin: "https://steward.example.com",
    exchangeUrl: "https://identity.example.com/v1/oidc/exchange",
    sourceHelper: "/example/source-helper",
    workflow: "release-summary@1",
  };
}

describe("caller HTTP journey helpers", () => {
  test("credential-bearing URLs are exact and verified", () => {
    expect(exactHttpUrl("exchange", "https://identity.example.com/v1/oidc/exchange", { path: "/v1/oidc/exchange" }).hostname)
      .toBe("identity.example.com");
    expect(exactHttpUrl("local", "http://127.0.0.1:8080").origin).toBe("http://127.0.0.1:8080");
    for (const value of [
      "http://identity.example.com/v1/oidc/exchange",
      "https://alice@identity.example.com/v1/oidc/exchange",
      "https://identity.example.com/v1/oidc/exchange?service=admin",
    ]) {
      expect(() => exactHttpUrl("exchange", value)).toThrow();
    }
  });

  test("the source helper returns one opaque value without surfacing stderr", () => {
    const spawn = () => ({ error: null, signal: null, status: 0, stdout: "obviously-fake-source\n", stderr: "" });
    expect(sourceAssertionFromHelper("/example/source-helper", spawn)).toBe("obviously-fake-source");
    const failed = () => ({ error: null, signal: null, status: 1, stdout: "", stderr: "obviously-fake-secret" });
    expect(() => sourceAssertionFromHelper("/example/source-helper", failed)).toThrow("did not produce a credential");
    try {
      sourceAssertionFromHelper("/example/source-helper", failed);
    } catch (error) {
      expect(error.message).not.toContain("obviously-fake-secret");
    }
  });

  test("the live Jira helper receives no arguments and exposes no output", () => {
    let invocation;
    runLiveJiraHelper("/example/jira-helper", (command, arguments_, options) => {
      invocation = { command, arguments_, options };
      return { error: null, signal: null, status: 0 };
    });
    expect(invocation.command).toBe("/example/jira-helper");
    expect(invocation.arguments_).toEqual([]);
    expect(invocation.options.stdio).toBe("ignore");
  });

  test("live Jira authorization is bound to the current run prefix", () => {
    expect(() => requireLiveJiraAuthorization({
      STEWARD_JOURNEY_ALLOW_LIVE_JIRA_WEBHOOK: "journey-001",
      STEWARD_JOURNEY_RUN_PREFIX: "journey-001",
    })).not.toThrow();
    expect(() => requireLiveJiraAuthorization({
      STEWARD_JOURNEY_ALLOW_LIVE_JIRA_WEBHOOK: "journey-previous",
      STEWARD_JOURNEY_RUN_PREFIX: "journey-001",
    })).toThrow("this run prefix");
  });

  test("the unbound assertion can target only a supported exchange path", () => {
    const environment = {
      STEWARD_JOURNEY_UNBOUND_EXCHANGE_URL: "https://identity.example.com/v1/workload/exchange",
      STEWARD_JOURNEY_UNBOUND_SOURCE_HELPER: "/example/source-helper",
    };
    expect(unboundConfiguration(environment).exchangeUrl).toBe("https://identity.example.com/v1/workload/exchange");
    expect(() => unboundConfiguration({
      ...environment,
      STEWARD_JOURNEY_UNBOUND_EXCHANGE_URL: "https://identity.example.com/admin",
    })).toThrow("supported exchange path");
  });

  test("exchange sends an empty request and validates the short-lived response", async () => {
    let request;
    const token = await exchangeTaskToken(config(), {
      sourceAssertion: () => "obviously-fake-source",
      fetchImpl: async (url, options) => {
        request = { url: url.toString(), options };
        return jsonResponse({ access_token: "obviously-fake-task", token_type: "Bearer", expires_in: 60 });
      },
    });
    expect(token).toBe("obviously-fake-task");
    expect(request.url).toBe("https://identity.example.com/v1/oidc/exchange");
    expect(request.options.body).toBe("");
    expect(request.options.redirect).toBe("manual");
    expect(request.options.headers.authorization).toBe("Bearer obviously-fake-source");
  });

  test("a refused exchange exposes only its status", async () => {
    const responseBody = "obviously-fake-secret";
    let caught;
    try {
      await exchangeTaskToken(config(), {
        sourceAssertion: () => "obviously-fake-source",
        fetchImpl: async () => new Response(responseBody, { status: 403 }),
      });
    } catch (error) {
      caught = error;
    }
    expect(caught).toBeInstanceOf(ExchangeRefusedError);
    expect(caught.message).not.toContain(responseBody);
  });

  test("Task lifecycle calls exchange afresh and uses the stable identity", async () => {
    const requests = [];
    let tokenNumber = 0;
    const fetchImpl = async (url, options) => {
      const path = new URL(url).pathname;
      requests.push({ path, options });
      if (path.endsWith("/exchange")) {
        tokenNumber += 1;
        return jsonResponse({ access_token: `obviously-fake-task-${tokenNumber}`, token_type: "Bearer", expires_in: 60 });
      }
      if (path === "/v1/tasks" && options.method === "POST") return jsonResponse({ taskUid: "11111111-1111-4111-8111-111111111111", phase: "submitted" }, 202);
      if (path.endsWith("/inputs")) return new Response(null, { status: 204 });
      if (path.endsWith("/execute")) return new Response(null, { status: 202 });
      if (path.endsWith("/outputs")) return new Response(Buffer.from("caller journey complete"), { status: 200, headers: { "content-type": "application/x-tar" } });
      if (options.method === "DELETE") return jsonResponse({ phase: "succeeded" }, 202);
      return jsonResponse({ taskUid: "11111111-1111-4111-8111-111111111111", phase: "succeeded" });
    };
    const dependencies = { fetchImpl, sourceAssertion: () => "obviously-fake-source", sleep: async () => {} };
    const task = await startCallerTask(config(), "jenkins:example-job:101", dependencies);
    const terminal = await waitForCallerTask(config(), task.taskUid, 1_000, dependencies);
    const output = await downloadCallerOutput(config(), task.taskUid, dependencies);
    await deleteCallerTask(config(), task.taskUid, dependencies);

    expect(terminal.phase).toBe("succeeded");
    expect(outputArchiveContains(output, "caller journey complete")).toBeTrue();
    expect(tokenNumber).toBe(6);
    const create = requests.find((entry) => entry.path === "/v1/tasks");
    expect(create.options.headers["idempotency-key"]).toBe("jenkins:example-job:101");
    const upload = requests.find((entry) => entry.path.endsWith("/inputs"));
    expect(upload.options.body.length).toBe(1024);
    const taskRequests = requests.filter((entry) => !entry.path.endsWith("/exchange"));
    expect(new Set(taskRequests.map((entry) => entry.options.headers.authorization)).size).toBe(taskRequests.length);
  });

  test("Task cleanup is registered before an input upload can fail", async () => {
    let createdTaskUid;
    const fetchImpl = async (url) => {
      const path = new URL(url).pathname;
      if (path.endsWith("/exchange")) {
        return jsonResponse({ access_token: "obviously-fake-task", token_type: "Bearer", expires_in: 60 });
      }
      if (path === "/v1/tasks") {
        return jsonResponse({ taskUid: "11111111-1111-4111-8111-111111111111", phase: "submitted" }, 202);
      }
      if (path.endsWith("/inputs")) return new Response(null, { status: 500 });
      throw new Error(`unexpected request ${path}`);
    };
    await expect(startCallerTask(config(), "jenkins:example-job:102", {
      fetchImpl,
      onCreated: async (taskUid) => { createdTaskUid = taskUid; },
      sourceAssertion: () => "obviously-fake-source",
    })).rejects.toThrow("input archive");
    expect(createdTaskUid).toBe("11111111-1111-4111-8111-111111111111");
  });

  test("Jira deliveries are signed over the exact replayed body", async () => {
    const body = jiraPayload("PROJ-123", "journey-001", "delivery-123");
    const signature = jiraSignature(Buffer.from("obviously-fake-signing-secret"), body);
    let request;
    const response = await sendJiraDelivery({
      origin: "https://receiver.example.com",
      secret: Buffer.from("obviously-fake-signing-secret"),
      trigger: "jira-read",
    }, "delivery-123", body, {
      fetchImpl: async (url, options) => {
        request = { url: url.toString(), options };
        return new Response("The event was accepted.", { status: 202 });
      },
    });
    expect(response.status).toBe(202);
    expect(request.url).toBe("https://receiver.example.com/hooks/jira/jira-read");
    expect(request.options.headers["x-hub-signature"]).toBe(signature);
    expect(Buffer.from(request.options.body)).toEqual(body);
  });
});
