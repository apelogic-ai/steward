import { expect } from "@playwright/test";

import {
  ExchangeRefusedError,
  callerConfiguration,
  deleteCallerTask,
  downloadCallerOutput,
  exactHttpUrl,
  exchangeTaskToken,
  invalidJiraSignature,
  jiraPayload,
  jiraReceiverConfiguration,
  outputArchiveContains,
  requireLiveJiraAuthorization,
  runLiveJiraHelper,
  sendJiraDelivery,
  startCallerTask,
  unboundConfiguration,
  waitForCallerTask,
} from "../caller-http.mjs";
import { Given, Then, When } from "./fixtures.mjs";

function requiredEnvironment(name) {
  const value = process.env[name]?.trim();
  if (!value) throw new Error(`${name} is required for @callers journeys.`);
  return value;
}

function targetOrigin() {
  return exactHttpUrl(
    "STEWARD_JOURNEY_BASE_URL",
    requiredEnvironment("STEWARD_JOURNEY_BASE_URL"),
  ).origin;
}

async function adminRuns(page) {
  const runs = [];
  const cursors = new Set();
  let cursor = null;
  do {
    const query = new URLSearchParams({ limit: "100" });
    if (cursor) query.set("cursor", cursor);
    const response = await page.request.get(`${targetOrigin()}/admin/api/v1/runs?${query}`);
    expect(response.ok(), "the caller journey requires a signed-in Steward administrator session").toBeTruthy();
    const body = await response.json();
    expect(body.apiVersion).toBe("steward.admin/runs/v1");
    expect(Array.isArray(body.runs)).toBeTruthy();
    runs.push(...body.runs);
    cursor = body.nextCursor ?? null;
    if (cursor) {
      expect(cursors.has(cursor), "the administrator Run cursor must advance").toBeFalsy();
      cursors.add(cursor);
    }
  } while (cursor);
  return runs;
}

async function recordInitialRuns(journey, page) {
  journey.callerInitialRunIds = new Set((await adminRuns(page)).map((run) => run.taskUid));
  journey.receiverNewRuns = [];
}

async function newRuns(journey, page) {
  expect(journey.callerInitialRunIds, "the journey must record existing Tasks before sending a request").toBeTruthy();
  return (await adminRuns(page)).filter((run) => !journey.callerInitialRunIds.has(run.taskUid));
}

async function expectNewRunCount(journey, page, expected) {
  if (expected === 0) {
    await page.waitForTimeout(2_000);
    journey.receiverNewRuns = await newRuns(journey, page);
    expect(journey.receiverNewRuns, "a refused or replayed request must not create a Task").toHaveLength(0);
    return;
  }
  await expect.poll(async () => {
    journey.receiverNewRuns = await newRuns(journey, page);
    return journey.receiverNewRuns.length;
  }, {
    message: `the receiver should create exactly ${expected} new Task(s)`,
    timeout: 30_000,
  }).toBe(expected);
}

async function sendConfiguredJiraDelivery(journey, deliveryId, issueKey, dependencies = {}) {
  const body = jiraPayload(issueKey, requiredEnvironment("STEWARD_JOURNEY_RUN_PREFIX"), deliveryId);
  const response = await sendJiraDelivery(journey.receiverConfig, deliveryId, body, dependencies);
  journey.receiverDeliveryId = deliveryId;
  journey.receiverPayload = body;
  journey.receiverResponse = { body: await response.text(), status: response.status };
}

Given("the caller journey target is configured", async ({ journey }) => {
  targetOrigin();
  requiredEnvironment("STEWARD_JOURNEY_RUN_PREFIX");
  expect(Array.isArray(journey.cleanups)).toBeTruthy();
});

Given("the {string} caller is bound to a versioned Workflow", async ({ journey }, caller) => {
  journey.callerConfig = callerConfiguration(caller);
  journey.callerName = caller;
});

When("a Jenkins job exchanges its source assertion", async ({ journey }) => {
  expect(journey.callerName).toBe("Jenkins");
  await exchangeTaskToken(journey.callerConfig);
  journey.callerExchangeSucceeded = true;
});

When("a Temporal activity exchanges its workload assertion", async ({ journey }) => {
  expect(journey.callerName).toBe("Temporal");
  await exchangeTaskToken(journey.callerConfig);
  journey.callerExchangeSucceeded = true;
});

When("it runs the bound Workflow with the stable identity {string}", async ({ journey }, identity) => {
  expect(journey.callerExchangeSucceeded, "the caller must exchange its source identity before starting a Task").toBeTruthy();
  const resolvedIdentity = identity.replaceAll("<run-prefix>", requiredEnvironment("STEWARD_JOURNEY_RUN_PREFIX"));
  const config = journey.callerConfig;
  journey.callerTask = await startCallerTask(config, resolvedIdentity, {
    onCreated: (taskUid) => {
      journey.cleanups.push(() => deleteCallerTask(config, taskUid));
    },
  });
});

Then("the caller Task ends as {string} within {int} min", async ({ journey }, phase, minutes) => {
  expect(journey.callerTask?.taskUid, "the caller must start a Task before waiting for it").toBeTruthy();
  journey.callerTerminal = await waitForCallerTask(journey.callerConfig, journey.callerTask.taskUid, minutes * 60_000);
  expect(journey.callerTerminal.phase, journey.callerTerminal.failureReason ?? "the caller Task ended in the wrong phase").toBe(phase);
});

Then("the caller downloads output containing {string}", async ({ journey }, expected) => {
  expect(journey.callerTerminal?.phase).toBe("succeeded");
  const output = await downloadCallerOutput(journey.callerConfig, journey.callerTask.taskUid);
  expect(outputArchiveContains(output, expected), "the downloaded output archive should contain the expected result").toBeTruthy();
});

Given("an unbound caller assertion is available", async ({ journey, page }) => {
  journey.unboundConfig = unboundConfiguration();
  await recordInitialRuns(journey, page);
});

When("the unbound caller requests a Task token", async ({ journey }) => {
  try {
    await exchangeTaskToken(journey.unboundConfig);
    journey.unboundRefused = false;
  } catch (error) {
    if (!(error instanceof ExchangeRefusedError)) throw error;
    journey.unboundRefused = true;
  }
});

Then("the token exchange is refused", async ({ journey }) => {
  expect(journey.unboundRefused, "the unbound caller must receive a client refusal from token exchange").toBeTruthy();
});

Then(/^the caller creates (\d+) new Tasks$/, async ({ journey, page }, expectedText) => {
  await expectNewRunCount(journey, page, Number(expectedText));
});

Given("the Jira receiver uses a read-only Envelope", async ({ journey, page }) => {
  journey.receiverConfig = jiraReceiverConfiguration("read");
  await recordInitialRuns(journey, page);
});

Given("the Jira receiver uses a write-capable Envelope", async ({ journey, page }) => {
  journey.receiverConfig = jiraReceiverConfiguration("write");
  await recordInitialRuns(journey, page);
});

Given("the owner explicitly allowed a live Jira webhook test", async () => {
  requireLiveJiraAuthorization();
});

When("Jira sends the signed delivery {string} for issue {string}", async ({ journey }, deliveryId, issueKey) => {
  await sendConfiguredJiraDelivery(journey, deliveryId, issueKey);
});

When("Jira sends the delivery {string} for issue {string} with an invalid signature", async ({ journey }, deliveryId, issueKey) => {
  const body = jiraPayload(issueKey, requiredEnvironment("STEWARD_JOURNEY_RUN_PREFIX"), deliveryId);
  const signature = invalidJiraSignature(journey.receiverConfig.secret, body);
  const response = await sendJiraDelivery(journey.receiverConfig, deliveryId, body, { signature });
  journey.receiverDeliveryId = deliveryId;
  journey.receiverPayload = body;
  journey.receiverResponse = { body: await response.text(), status: response.status };
});

When("Jira sends the delivery {string} for issue {string} without a signature", async ({ journey }, deliveryId, issueKey) => {
  await sendConfiguredJiraDelivery(journey, deliveryId, issueKey, { signature: null });
});

When("Jira replays that signed delivery", async ({ journey }) => {
  expect(journey.receiverPayload, "the first signed Jira delivery must be sent before replaying it").toBeTruthy();
  const response = await sendJiraDelivery(
    journey.receiverConfig,
    journey.receiverDeliveryId,
    journey.receiverPayload,
  );
  journey.receiverResponse = { body: await response.text(), status: response.status };
});

When("the live Jira helper updates issue {string}", async ({}, issueKey) => {
  expect(requiredEnvironment("STEWARD_JOURNEY_LIVE_JIRA_ISSUE_KEY")).toBe(issueKey);
  runLiveJiraHelper(requiredEnvironment("STEWARD_JOURNEY_LIVE_JIRA_HELPER"));
});

Then("the receiver accepts the delivery", async ({ journey }) => {
  expect(journey.receiverResponse).toEqual({ body: "The event was accepted.", status: 202 });
});

Then("the receiver refuses the delivery because the Envelope is not read-only", async ({ journey }) => {
  expect(journey.receiverResponse).toEqual({ body: "This Trigger is not read-only.", status: 400 });
});

Then("the receiver refuses the delivery because its signature is invalid", async ({ journey }) => {
  expect(journey.receiverResponse).toEqual({ body: "The request signature is invalid.", status: 401 });
});

Then("the receiver reports that no run was needed", async ({ journey }) => {
  expect(journey.receiverResponse).toEqual({ body: "No run was needed.", status: 200 });
});

Then("the receiver reports that it cannot start another run yet", async ({ journey }) => {
  expect(journey.receiverResponse).toEqual({
    body: "This Trigger cannot start another run yet.",
    status: 429,
  });
});

Then(/^the receiver creates (\d+) new Tasks?$/, async ({ journey, page }, expectedText) => {
  await expectNewRunCount(journey, page, Number(expectedText));
});

Then("the receiver Task ends as {string} within {int} min", async ({ journey, page }, phase, minutes) => {
  expect(journey.receiverNewRuns).toHaveLength(1);
  const taskUid = journey.receiverNewRuns[0].taskUid;
  let run;
  await expect.poll(async () => {
    const response = await page.request.get(`${targetOrigin()}/admin/api/v1/runs/${encodeURIComponent(taskUid)}`);
    expect(response.ok(), "the receiver-created Task should remain visible to an administrator").toBeTruthy();
    run = (await response.json()).run;
    return `${run.phase}:${run.finalized === true}`;
  }, {
    message: "the receiver-created Task should reach its terminal phase and finish cleanup",
    timeout: minutes * 60_000,
  }).toBe(`${phase}:true`);
  expect(run.phase, run.failureReason ?? "the receiver-created Task ended in the wrong phase").toBe(phase);
  expect(run.finalized, "the receiver-created Task should finish cleanup").toBe(true);
});
