import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { renderToStaticMarkup } from "react-dom/server";

import { classifyResource } from "@/data/use-api-resource";
import {
  NewPersonalAgentForm,
  PersonalAgentDetailContent,
  PersonalAgentsListContent,
  activeEnvelopeChoices,
  nextPersonalAgentMessageKey,
  personalAgentFailureMessage,
  subscribePersonalAgentEvents,
  type PersonalAgentThread,
} from "./personal-agent-views";

const thread: PersonalAgentThread = {
  agent: {
    agentId: "agent-1",
    agentRef: "codex@configured",
    createdAt: "2026-10-19T05:00:00Z",
    envelopeRequestId: "envelope-active",
    instructions: "Keep replies concise.",
    name: "Release helper",
    updatedAt: "2026-10-19T05:04:00Z",
  },
  turns: [{
    messages: [
      { content: "Summarize the release.", createdAt: "2026-10-19T05:01:00Z", role: "user" },
      { content: "The release is ready.", createdAt: "2026-10-19T05:04:00Z", role: "agent" },
    ],
    runId: "run-1",
    status: "done",
    turnId: "turn-1",
    updatedAt: "2026-10-19T05:04:00Z",
  }],
};

const source = readFileSync(new URL("./personal-agent-views.tsx", import.meta.url), "utf8");

describe("personal agent creation", () => {
  test("offers only active Envelopes and keeps their configured labels", () => {
    expect(activeEnvelopeChoices([
      { envelopeDigest: "sha256:active", id: "envelope-active", status: "provisioned", templateDisplayName: "Standard" },
      { envelopeDigest: null, id: "envelope-pending", status: "pending", templateDisplayName: "Pending" },
      { envelopeDigest: "sha256:stale", id: "envelope-stale", status: "stale", templateDisplayName: "Stale" },
    ])).toEqual([{ label: "Standard", requestId: "envelope-active" }]);
  });

  test("shows a plain failure reason supplied by the API", () => {
    expect(personalAgentFailureMessage({ failureReason: "Choose an active Envelope." })).toBe("Choose an active Envelope.");
    expect(personalAgentFailureMessage({ error: "unprocessable_entity" })).toBe("Could not save this agent. Try again.");
  });

  test("labels every required field and disables creation without an active Envelope", () => {
    const markup = renderToStaticMarkup(<NewPersonalAgentForm
      envelopeChoices={[]}
      harnessChoices={[{ agentRef: "codex@configured", label: "Codex" }]}
      onSubmit={() => undefined}
      saving={false}
    />);
    expect(markup).toContain("Name");
    expect(markup).toContain("Coding agent");
    expect(markup).toContain("Instructions");
    expect(markup).toContain("Envelope");
    expect(markup).toContain("disabled");
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
  test("uses the owner-approved agent API routes", () => {
    expect(source).toContain('personalAgentRequest("/app/api/v1/agents")');
    expect(source).toContain("/app/api/v1/agents/${encodeURIComponent(agentId)}");
    expect(source).not.toContain("/app/api/v1/personal-agents");
  });

  test("uses a browser event stream instead of polling for turn updates", () => {
    expect(source).toContain("new EventSource");
    expect(source).not.toContain("setTimeout");
  });

  test("keeps one idempotency key while retrying the same message", () => {
    expect(nextPersonalAgentMessageKey("message-1", () => "message-2")).toBe("message-1");
    expect(nextPersonalAgentMessageKey(null, () => "message-2")).toBe("message-2");
  });

  test("opens one resumable browser stream only after the agent is found", () => {
    let opened = 0;
    let closed = 0;
    let path = "";
    let withCredentials = false;
    const createEventSource = (nextPath: string, init: EventSourceInit) => {
      opened += 1;
      path = nextPath;
      withCredentials = init.withCredentials ?? false;
      return { close: () => { closed += 1; }, onmessage: null };
    };

    const missing = classifyResource<PersonalAgentThread>({ response: new Response(null, { status: 404 }) });
    expect(subscribePersonalAgentEvents(missing.status, "not-owned", () => undefined, createEventSource)).toBeUndefined();
    expect(opened).toBe(0);

    const close = subscribePersonalAgentEvents("ready", "agent/1", () => undefined, createEventSource);
    expect(opened).toBe(1);
    expect(path).toBe("/app/api/v1/agents/agent%2F1/events");
    expect(withCredentials).toBe(true);
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
