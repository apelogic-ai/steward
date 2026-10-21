import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { renderToStaticMarkup } from "react-dom/server";

import { classifyResource } from "@/data/use-api-resource";
import {
  NewPersonalAgentForm,
  PersonalAgentDetailContent,
  PersonalAgentsListContent,
  activeEnvelopeChoices,
  compatiblePersonalAgentModels,
  isCurrentPersonalAgentRefresh,
  loadProvisionedEnvelopeRequests,
  nextPersonalAgentMessageAttempt,
  personalAgentFailureMessage,
  selectedPersonalAgentModel,
  subscribePersonalAgentEvents,
  type PersonalAgentThread,
} from "./personal-agent-views";

const thread: PersonalAgentThread = {
  agent: {
    agentRef: "codex@configured",
    createdAt: "2026-10-19T05:00:00Z",
    envelopeRequestId: "envelope-active",
    id: "agent-1",
    instructions: "Keep replies concise.",
    model: { provider: "example", model: "model-a" },
    name: "Release helper",
    updatedAt: "2026-10-19T05:04:00Z",
  },
  turns: [{
    completedAt: "2026-10-19T05:04:00Z",
    createdAt: "2026-10-19T05:01:00Z",
    failureReason: null,
    id: "turn-1",
    messages: [{ id: "message-1", content: "Summarize the release.", createdAt: "2026-10-19T05:01:00Z" }],
    reply: { content: "The release is ready.", createdAt: "2026-10-19T05:04:00Z" },
    runId: "run-1",
    startedAt: "2026-10-19T05:02:00Z",
    status: "done",
  }],
};

const source = readFileSync(new URL("./personal-agent-views.tsx", import.meta.url), "utf8");

describe("personal agent creation", () => {
  test("loads every page of active Envelopes", async () => {
    const cursors: Array<string | undefined> = [];
    const result = await loadProvisionedEnvelopeRequests(async (cursor) => {
      cursors.push(cursor);
      return {
        data: {
          apiVersion: "steward.envelope-requests/v1",
          nextCursor: cursor ? null : "next-page",
          requests: [{ id: cursor ? "envelope-2" : "envelope-1", status: "provisioned" } as never],
        },
        response: new Response(null, { status: 200 }),
      };
    });

    expect(cursors).toEqual([undefined, "next-page"]);
    expect(result.data?.requests.map((request) => request.id)).toEqual(["envelope-1", "envelope-2"]);
    expect(result.data?.nextCursor).toBeNull();
  });

  test("offers only active Envelopes and keeps their configured labels", () => {
    expect(activeEnvelopeChoices([
      {
        envelopeDigest: "sha256:active",
        id: "envelope-active",
        requestedEnvelope: { spec: { llms: [{ provider: "example", model: "model-a" }] } },
        status: "provisioned",
        templateDisplayName: "Standard",
      },
      { envelopeDigest: null, id: "envelope-pending", status: "pending", templateDisplayName: "Pending" },
      { envelopeDigest: "sha256:stale", id: "envelope-stale", status: "stale", templateDisplayName: "Stale" },
    ])).toEqual([{
      label: "Standard",
      models: [{ provider: "example", model: "model-a" }],
      requestId: "envelope-active",
    }]);
  });

  test("shows a plain failure reason supplied by the API", () => {
    expect(personalAgentFailureMessage({ failureReason: "Choose an active Envelope." })).toBe("Choose an active Envelope.");
    expect(personalAgentFailureMessage({ error: "unprocessable_entity" })).toBe("Could not save this agent. Try again.");
  });

  test("labels every required field and disables creation without an active Envelope", () => {
    const markup = renderToStaticMarkup(<NewPersonalAgentForm
      envelopeChoices={[]}
      harnessChoices={[{ agentRef: "codex@configured", label: "Codex", providers: ["example"] }]}
      onSubmit={() => undefined}
      saving={false}
    />);
    expect(markup).toContain("Name");
    expect(markup).toContain("Coding agent");
    expect(markup).toContain("Instructions");
    expect(markup).toContain("Envelope");
    expect(markup).toContain("disabled");
  });

  test("asks for a model only when the selected Envelope offers more than one", () => {
    const oneModel = renderToStaticMarkup(<NewPersonalAgentForm
      envelopeChoices={[{
        label: "Standard",
        models: [{ provider: "example", model: "model-a" }],
        requestId: "envelope-one",
      }]}
      harnessChoices={[{ agentRef: "codex@configured", label: "Codex", providers: ["example"] }]}
      onSubmit={() => undefined}
      saving={false}
    />);
    const multipleModels = renderToStaticMarkup(<NewPersonalAgentForm
      envelopeChoices={[{
        label: "Flexible",
        models: [
          { provider: "example", model: "model-a" },
          { provider: "future-provider", model: "future-model" },
        ],
        requestId: "envelope-multiple",
      }]}
      harnessChoices={[{ agentRef: "codex@configured", label: "Codex", providers: ["example", "future-provider"] }]}
      onSubmit={() => undefined}
      saving={false}
    />);

    expect(oneModel).not.toContain("personal-agent-model");
    expect(multipleModels).toContain("personal-agent-model");
    expect(multipleModels).toContain("example/model-a");
    expect(multipleModels).toContain("future-provider/future-model");
  });

  test("offers only models supported by the selected coding agent", () => {
    const models = [
      { provider: "provider-a", model: "model-a" },
      { provider: "provider-b", model: "model-b" },
    ];
    expect(compatiblePersonalAgentModels(models, ["provider-b"])).toEqual([
      { provider: "provider-b", model: "model-b" },
    ]);
  });

  test("sends an explicit model only for an Envelope with multiple choices", () => {
    const choices = [{
      label: "Flexible",
      models: [
        { provider: "example", model: "model-a" },
        { provider: "future-provider", model: "future-model" },
      ],
      requestId: "envelope-multiple",
    }];

    expect(selectedPersonalAgentModel(
      choices,
      "envelope-multiple",
      JSON.stringify(["future-provider", "future-model"]),
    )).toEqual({ provider: "future-provider", model: "future-model" });
    expect(selectedPersonalAgentModel(
      [{ ...choices[0], models: [choices[0].models[0]] }],
      "envelope-multiple",
      JSON.stringify(["example", "model-a"]),
    )).toBeUndefined();
  });
});

describe("personal agent list", () => {
  test("links each agent thread and shows its configured coding agent", () => {
    const markup = renderToStaticMarkup(<PersonalAgentsListContent state={{
      status: "ready",
      value: { agents: [thread.agent], apiVersion: "steward.personal-agents/v1" },
    }} />);
    expect(markup).toContain("My agents");
    expect(markup).toContain("Release helper");
    expect(markup).toContain("codex@configured");
    expect(markup).toContain('href="/agents/agent-1"');
    expect(markup).toContain('href="/agents/new"');
  });

  test("offers creation when no agents exist", () => {
    const markup = renderToStaticMarkup(<PersonalAgentsListContent state={{
      status: "ready",
      value: { agents: [], apiVersion: "steward.personal-agents/v1" },
    }} />);
    expect(markup).toContain("No agents yet");
    expect(markup).toContain("Create your first agent");
  });
});

describe("personal agent thread", () => {
  test("uses the generated agent API client", () => {
    expect(source).toContain("listPersonalAgentsRequest({");
    expect(source).toContain("createPersonalAgentRequest({");
    expect(source).toContain("getPersonalAgentRequest({");
    expect(source).toContain("listPersonalAgentMessages({");
    expect(source).toContain("postPersonalAgentMessageRequest({");
    expect(source).not.toContain("fetch(");
  });

  test("uses a browser event stream instead of polling for turn updates", () => {
    expect(source).toContain("new EventSource");
    expect(source).not.toContain("setTimeout");
  });

  test("keeps one idempotency key while retrying the same normalized message", () => {
    const first = nextPersonalAgentMessageAttempt(null, "Repeat this", () => "message-1");
    expect(nextPersonalAgentMessageAttempt(first, "Repeat this", () => "message-2")).toEqual(first);
    expect(nextPersonalAgentMessageAttempt(first, "Different message", () => "message-2")).toEqual({
      content: "Different message",
      key: "message-2",
    });
  });

  test("applies only the latest completed thread refresh", () => {
    expect(isCurrentPersonalAgentRefresh(3, 3)).toBeTrue();
    expect(isCurrentPersonalAgentRefresh(3, 2)).toBeFalse();
  });

  test("opens one resumable browser stream only after the agent is found", () => {
    let opened = 0;
    let closed = 0;
    let path = "";
    let withCredentials = false;
    const eventNames: string[] = [];
    const eventHandlers: Array<() => void> = [];
    const createEventSource = (nextPath: string, init: EventSourceInit) => {
      opened += 1;
      path = nextPath;
      withCredentials = init.withCredentials ?? false;
      return {
        addEventListener: (eventName: string, handler: () => void) => {
          eventNames.push(eventName);
          eventHandlers.push(handler);
        },
        close: () => { closed += 1; },
      };
    };

    const missing = classifyResource<PersonalAgentThread>({ response: new Response(null, { status: 404 }) });
    expect(subscribePersonalAgentEvents(missing.status, "not-owned", () => undefined, createEventSource)).toBeUndefined();
    expect(opened).toBe(0);

    let refreshes = 0;
    const close = subscribePersonalAgentEvents("ready", "agent/1", () => { refreshes += 1; }, createEventSource);
    expect(opened).toBe(1);
    expect(path).toBe("/app/api/v1/agents/agent%2F1/events");
    expect(withCredentials).toBe(true);
    expect(eventNames).toEqual(["thread.snapshot", "turn.updated"]);
    eventHandlers.forEach((handler) => handler());
    expect(refreshes).toBe(2);
    close?.();
    expect(closed).toBe(1);
  });

  test("shows the ordered conversation, turn status, and existing Run link", () => {
    const markup = renderToStaticMarkup(<PersonalAgentDetailContent
      draft=""
      onDraftChange={() => undefined}
      onSend={() => undefined}
      sending={false}
      state={{ status: "ready", value: thread }}
    />);
    expect(markup).toContain("Release helper");
    expect(markup).toContain("Summarize the release.");
    expect(markup).toContain("The release is ready.");
    expect(markup).toContain("Done");
    expect(markup).toContain('data-tone="ok"');
    expect(markup).toContain('href="/runs/run-1"');
    expect(markup).toContain("Send message");
  });

  test("a missing or not-owned agent reveals no agent data or message controls", () => {
    const state = classifyResource<PersonalAgentThread>({ response: new Response(null, { status: 404 }) });
    const markup = renderToStaticMarkup(<PersonalAgentDetailContent
      draft=""
      onDraftChange={() => undefined}
      onSend={() => undefined}
      sending={false}
      state={state}
    />);
    expect(markup).toContain("Not found");
    expect(markup).not.toContain("Release helper");
    expect(markup).not.toContain("Send message");
    expect(markup).not.toContain("<form");
  });
});
