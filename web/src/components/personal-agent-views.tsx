"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";

import {
  createPersonalAgent as createPersonalAgentRequest,
  getPersonalAgent as getPersonalAgentRequest,
  getUserCapabilities,
  listPersonalAgentMessages,
  listPersonalAgents as listPersonalAgentsRequest,
  listRequests,
  postPersonalAgentMessage as postPersonalAgentMessageRequest,
  type EnvelopeRequestStatus,
  type EnvelopeRequestsResponse,
  type PersonalAgentMessageView,
  type PersonalAgentReplyView,
  type PersonalAgentsResponse,
  type PersonalAgentTurnView,
  type PersonalAgentView,
  type UserCapabilitiesResponse,
} from "@/api-client";
import { DataTable } from "@/components/hs/data-table";
import { SectionCard } from "@/components/hs/section-card";
import { StatusPill } from "@/components/hs/status-pill";
import { EmptyState, PageHeader, PrimaryLink, ResourceBoundary } from "@/components/workspace-ui";
import { classifyResource, useApiResource, type ResourceState } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

export type PersonalAgent = PersonalAgentView;
export type PersonalAgentMessage = PersonalAgentMessageView;
export type PersonalAgentReply = PersonalAgentReplyView;
export type PersonalAgentTurn = PersonalAgentTurnView;

export type PersonalAgentThread = {
  agent: PersonalAgent;
  turns: PersonalAgentTurn[];
};

export type PersonalAgents = PersonalAgentsResponse;

type PersonalAgentApiResult<T> = {
  data?: T;
  error?: unknown;
  response?: Response;
};

export type PersonalAgentEnvelopeCandidate = {
  envelopeDigest?: string | null;
  id: string;
  status: EnvelopeRequestStatus;
  templateDisplayName?: string | null;
};

export type PersonalAgentEnvelopeChoice = {
  label: string;
  requestId: string;
};

export function activeEnvelopeChoices(
  requests: ReadonlyArray<PersonalAgentEnvelopeCandidate>,
): PersonalAgentEnvelopeChoice[] {
  return requests.flatMap((request) => request.status === "provisioned" && request.envelopeDigest
    ? [{
        label: request.templateDisplayName?.trim() || `Envelope ${request.id}`,
        requestId: request.id,
      }]
    : []);
}

export function personalAgentFailureMessage(
  error: unknown,
  fallback = "Could not save this agent. Try again.",
): string {
  if (error && typeof error === "object") {
    const reason = (error as Record<string, unknown>).failureReason;
    if (typeof reason === "string" && reason.trim()) return reason;
  }
  return fallback;
}

export function nextPersonalAgentMessageKey(
  current: string | null,
  create: () => string,
): string {
  return current ?? create();
}

export function PersonalAgentsListContent({ state }: Readonly<{
  state: ResourceState<PersonalAgents>;
}>) {
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader
        actions={<PrimaryLink href="/agents/new">New agent</PrimaryLink>}
        description="Create personal agents and return to their conversations."
        title="My agents"
      />
      <ResourceBoundary state={state}>{({ agents }) => agents.length === 0
        ? <EmptyState title="No agents yet"><p>Create your first agent to start a conversation.</p><div className="mt-4"><PrimaryLink href="/agents/new">Create your first agent</PrimaryLink></div></EmptyState>
        : <DataTable
            ariaLabel="Personal agents"
            columns={[
              { key: "name", label: "Name", render: (agent) => <span className="font-semibold">{agent.name}</span> },
              { key: "harness", label: "Coding agent", render: (agent) => <span className="font-mono text-xs text-muted-ink">{agent.agentRef}</span> },
            ]}
            gridTemplateColumns="minmax(220px,1fr) minmax(220px,1fr)"
            minWidth="520px"
            rowHref={(agent) => `/agents/${encodeURIComponent(agent.id)}`}
            rowKey={(agent) => agent.id}
            rows={agents}
          />}
      </ResourceBoundary>
    </section>
  );
}

async function loadPersonalAgentThread(agentId: string): Promise<PersonalAgentApiResult<PersonalAgentThread>> {
  const [agent, messages] = await Promise.all([
    getPersonalAgentRequest({
      cache: "no-store",
      credentials: "same-origin",
      path: { agent_id: agentId },
    }),
    listPersonalAgentMessages({
      cache: "no-store",
      credentials: "same-origin",
      path: { agent_id: agentId },
    }),
  ]);
  const response = !agent.response?.ok ? agent.response : messages.response;
  return agent.data && messages.data
    ? { data: { agent: agent.data.agent, turns: messages.data.turns }, response }
    : { error: agent.error ?? messages.error, response };
}

export function PersonalAgentsView() {
  const load = useCallback(() => listPersonalAgentsRequest({
    cache: "no-store",
    credentials: "same-origin",
  }), []);
  const state = useApiResource(load);
  return <PersonalAgentsListContent state={state} />;
}

type NewPersonalAgentData = {
  capabilities: UserCapabilitiesResponse;
  envelopes: EnvelopeRequestsResponse;
};

export function NewPersonalAgentView() {
  const router = useRouter();
  const session = useSession();
  const [saving, setSaving] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const load = useCallback(async () => {
    const [capabilities, envelopes] = await Promise.all([
      getUserCapabilities({ cache: "no-store", credentials: "same-origin" }),
      listRequests({ cache: "no-store", credentials: "same-origin" }),
    ]);
    const response = !capabilities.response?.ok ? capabilities.response : envelopes.response;
    return capabilities.data && envelopes.data
      ? { data: { capabilities: capabilities.data, envelopes: envelopes.data }, response }
      : { response };
  }, []);
  const state = useApiResource<NewPersonalAgentData>(load);

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (session.status !== "authenticated") {
      setFailure("Sign in again, then retry.");
      return;
    }
    const form = new FormData(event.currentTarget);
    const body = {
      agentRef: String(form.get("agentRef") ?? "").trim(),
      envelopeRequestId: String(form.get("envelopeRequestId") ?? "").trim(),
      instructions: String(form.get("instructions") ?? "").trim(),
      name: String(form.get("name") ?? "").trim(),
    };
    setFailure(null);
    setSaving(true);
    const result = await createPersonalAgentRequest({
      body,
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
    });
    setSaving(false);
    if (result.data && result.response?.ok) {
      router.push(`/agents/${encodeURIComponent(result.data.agent.id)}`);
      return;
    }
    setFailure(personalAgentFailureMessage(result.error));
  };

  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Choose how this agent works and which active Envelope it may use." title="New agent" />
      <ResourceBoundary state={state}>{({ capabilities, envelopes }) => (
        <SectionCard title="Agent details">
          <NewPersonalAgentForm
            envelopeChoices={activeEnvelopeChoices(envelopes.requests)}
            harnessChoices={capabilities.agents.map((agent) => ({
              agentRef: agent.agentRef,
              label: agent.displayName?.trim() || agent.agentRef,
            }))}
            onSubmit={(event) => void submit(event)}
            saving={saving}
          />
          {failure ? <p className="mt-4 text-sm text-err" role="status">{failure}</p> : null}
        </SectionCard>
      )}</ResourceBoundary>
    </section>
  );
}

type PersonalAgentEventSource = {
  addEventListener: (eventName: string, handler: () => void) => void;
  close: () => void;
};

type PersonalAgentEventSourceFactory = (
  path: string,
  init: EventSourceInit,
) => PersonalAgentEventSource;

export function subscribePersonalAgentEvents(
  status: ResourceState<PersonalAgentThread>["status"],
  agentId: string,
  onEvent: () => void,
  createEventSource: PersonalAgentEventSourceFactory = (path, init) => {
    const source = new EventSource(path, init);
    return {
      addEventListener: (eventName, handler) => source.addEventListener(eventName, handler),
      close: () => source.close(),
    };
  },
): (() => void) | undefined {
  if (status !== "ready") return undefined;
  const source = createEventSource(`/app/api/v1/agents/${encodeURIComponent(agentId)}/events`, {
    withCredentials: true,
  });
  source.addEventListener("thread.snapshot", onEvent);
  source.addEventListener("turn.updated", onEvent);
  return () => source.close();
}

function usePersonalAgentThread(agentId: string): Readonly<{
  refresh: () => Promise<void>;
  state: ResourceState<PersonalAgentThread>;
}> {
  const [state, setState] = useState<ResourceState<PersonalAgentThread>>({ status: "loading" });
  const mounted = useRef(false);
  const refresh = useCallback(async () => {
    const next = classifyResource(await loadPersonalAgentThread(agentId));
    if (mounted.current) setState(next);
  }, [agentId]);

  useEffect(() => {
    mounted.current = true;
    void refresh();
    return () => {
      mounted.current = false;
    };
  }, [refresh]);

  useEffect(
    () => subscribePersonalAgentEvents(state.status, agentId, () => void refresh()),
    [agentId, refresh, state.status],
  );

  return { refresh, state };
}

export function PersonalAgentDetailView({ agentId }: Readonly<{ agentId: string }>) {
  const session = useSession();
  const [draft, setDraft] = useState("");
  const [failure, setFailure] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const idempotencyKey = useRef<string | null>(null);
  const { refresh, state } = usePersonalAgentThread(agentId);

  const updateDraft = (value: string) => {
    if (value !== draft) idempotencyKey.current = null;
    setDraft(value);
  };
  const send = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const content = draft.trim();
    if (!content) return;
    if (session.status !== "authenticated") {
      setFailure("Sign in again, then retry.");
      return;
    }
    idempotencyKey.current = nextPersonalAgentMessageKey(idempotencyKey.current, () => crypto.randomUUID());
    setFailure(null);
    setSending(true);
    const result = await postPersonalAgentMessageRequest({
      body: {
        content,
        idempotencyKey: idempotencyKey.current,
      },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": session.value.csrf },
      path: { agent_id: agentId },
    });
    setSending(false);
    if (result.data && result.response?.ok) {
      idempotencyKey.current = null;
      setDraft("");
      await refresh();
      return;
    }
    setFailure(personalAgentFailureMessage(result.error, "Could not send this message. Try again."));
  };

  return <PersonalAgentDetailContent
    draft={draft}
    failure={failure}
    onDraftChange={updateDraft}
    onSend={(event) => void send(event)}
    sending={sending}
    state={state}
  />;
}

function turnStatusLabel(status: PersonalAgentTurn["status"]): string {
  return {
    done: "Done",
    failed: "Failed",
    queued: "Queued",
    running: "Running",
  }[status];
}

function turnStatusTone(status: PersonalAgentTurn["status"]): "err" | "info" | "ok" {
  return status === "done" ? "ok" : status === "failed" ? "err" : "info";
}

function PersonalAgentThreadView({ thread }: Readonly<{ thread: PersonalAgentThread }>) {
  if (thread.turns.length === 0) {
    return <EmptyState title="Start the conversation"><p>Send a message to begin this agent&apos;s thread.</p></EmptyState>;
  }
  return (
    <div aria-label="Conversation" aria-live="polite" className="space-y-4" role="log">
      {thread.turns.map((turn) => (
        <SectionCard
          actions={<StatusPill tone={turnStatusTone(turn.status)} value={turnStatusLabel(turn.status)} />}
          key={turn.id}
          title="Turn"
        >
          <div className="space-y-4">
            {turn.messages.map((message) => (
              <article className="ms-auto max-w-3xl rounded-card bg-brand-soft px-4 py-3" key={message.id}>
                <p className="text-xs font-semibold text-muted-ink">You</p>
                <p className="mt-1 whitespace-pre-wrap text-sm leading-6">{message.content}</p>
              </article>
            ))}
            {turn.reply ? (
              <article className="max-w-3xl rounded-card bg-subtle px-4 py-3">
                <p className="text-xs font-semibold text-muted-ink">{thread.agent.name}</p>
                <p className="mt-1 whitespace-pre-wrap text-sm leading-6">{turn.reply.content}</p>
              </article>
            ) : null}
            {turn.status === "failed" && turn.failureReason ? <p className="text-sm text-err" role="status">{turn.failureReason}</p> : null}
            {turn.runId ? <Link className="inline-flex min-h-10 items-center rounded-control border px-4 text-sm font-semibold hover:bg-subtle" href={`/runs/${encodeURIComponent(turn.runId)}`}>Open Run</Link> : null}
          </div>
        </SectionCard>
      ))}
    </div>
  );
}

export function PersonalAgentDetailContent({ draft, failure, onDraftChange, onSend, sending, state }: Readonly<{
  draft: string;
  failure?: string | null;
  onDraftChange: (value: string) => void;
  onSend: (event: FormEvent<HTMLFormElement>) => void;
  sending: boolean;
  state: ResourceState<PersonalAgentThread>;
}>) {
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <ResourceBoundary state={state}>{(thread) => (
        <>
          <PageHeader description={thread.agent.instructions} title={thread.agent.name} />
          <PersonalAgentThreadView thread={thread} />
          <SectionCard title="Send a message">
            <form className="space-y-3" onSubmit={onSend}>
              <label className="grid gap-2 text-sm font-semibold" htmlFor="personal-agent-message">
                Message
                <textarea
                  className="min-h-28 w-full rounded-control border bg-panel px-3 py-2 font-normal"
                  disabled={sending}
                  id="personal-agent-message"
                  name="content"
                  onChange={(event) => onDraftChange(event.target.value)}
                  required
                  value={draft}
                />
              </label>
              <button className="inline-flex min-h-10 items-center rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:cursor-not-allowed disabled:opacity-60" disabled={sending || !draft.trim()} type="submit">
                {sending ? "Sending…" : "Send message"}
              </button>
              {failure ? <p className="text-sm text-err" role="status">{failure}</p> : null}
            </form>
          </SectionCard>
        </>
      )}</ResourceBoundary>
    </section>
  );
}

export function NewPersonalAgentForm({
  envelopeChoices,
  harnessChoices,
  onSubmit,
  saving,
}: Readonly<{
  envelopeChoices: ReadonlyArray<PersonalAgentEnvelopeChoice>;
  harnessChoices: ReadonlyArray<{ agentRef: string; label: string }>;
  onSubmit: (event: FormEvent<HTMLFormElement>) => void;
  saving: boolean;
}>) {
  const ready = envelopeChoices.length > 0 && harnessChoices.length > 0;
  return (
    <form className="space-y-5" onSubmit={onSubmit}>
      <label className="grid gap-2 text-sm font-semibold" htmlFor="personal-agent-name">
        Name
        <input className="min-h-11 rounded-control border bg-panel px-3 font-normal" disabled={saving} id="personal-agent-name" name="name" required />
      </label>
      <label className="grid gap-2 text-sm font-semibold" htmlFor="personal-agent-harness">
        Coding agent
        <select className="min-h-11 rounded-control border bg-panel px-3 font-normal" disabled={saving || harnessChoices.length === 0} id="personal-agent-harness" name="agentRef" required>
          {harnessChoices.map((choice) => <option key={choice.agentRef} value={choice.agentRef}>{choice.label}</option>)}
        </select>
      </label>
      <label className="grid gap-2 text-sm font-semibold" htmlFor="personal-agent-instructions">
        Instructions
        <textarea className="min-h-36 rounded-control border bg-panel px-3 py-2 font-normal" disabled={saving} id="personal-agent-instructions" name="instructions" required />
      </label>
      <label className="grid gap-2 text-sm font-semibold" htmlFor="personal-agent-envelope">
        Envelope
        <select className="min-h-11 rounded-control border bg-panel px-3 font-normal" disabled={saving || envelopeChoices.length === 0} id="personal-agent-envelope" name="envelopeRequestId" required>
          {envelopeChoices.map((choice) => <option key={choice.requestId} value={choice.requestId}>{choice.label}</option>)}
        </select>
      </label>
      {!ready ? <p className="text-sm text-muted-ink" role="status">You need an active Envelope and an available coding agent before you can create an agent.</p> : null}
      <button className="inline-flex min-h-10 items-center rounded-control bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:cursor-not-allowed disabled:opacity-60" disabled={saving || !ready} type="submit">
        {saving ? "Creating…" : "Create agent"}
      </button>
    </form>
  );
}
