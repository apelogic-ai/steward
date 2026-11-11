# Concepts

Six ideas explain almost everything you see in Steward. You only need the first two to start a
run.

## Envelope

An **Envelope** is the most you are allowed to use. An administrator approves it for you, and it
lists:

- **models** the agent may call, written as `provider` and `model`;
- **tools** it may use, each written as `provider`, `resource` and `action`, for example
  `github` / `actions` / `read`, or `cli` / `kubectl` / `read` for governed `kubectl` commands;
- a **budget**: a monthly limit, an optional limit per run, and a currency;
- a **maximum run duration** (TTL), and optionally monthly runtime minutes;
- **runner** limits: platforms, memory, compute and storage;
- **workspace** access: whether runs may check out repositories or use scratch space, and how
  much.

You get an Envelope by requesting one on the **Envelopes** page. Most requests start from a
template that an administrator prepared. A request that asks for more than the template needs a
short justification. An administrator approves or rejects it in Steward.

You can have more than one active Envelope. Each one has a digest such as
`steward:sha256:…`. When you have several, choose one: the browser asks you, and API callers pass
`envelopeDigest`. Without a choice, Steward refuses the run instead of guessing.

## Task requirements

An Envelope is a ceiling. A Task's **requirements** say how much of it this Task needs. They are
the optional `requires` object in the Task definition, or the **Requirements (optional JSON)**
field when you create a Task in the browser.

- **Omitted:** the run may use everything your Envelope allows.
- **Present:** the run gets exactly what you list. It must be complete, with every one of `llms`,
  `tools`, `budget`, `ttl` and `runner`; an empty list means "none". Each value must fit inside
  your Envelope, or the run is refused before it starts.

<!-- schema: directRequirements -->
```json
{
  "authority": {
    "llms": [{ "provider": "litellm", "model": "review-model" }],
    "tools": [{ "provider": "github", "resource": "actions", "action": "read" }],
    "budget": { "monthlyLimit": "50.00", "singleRunLimit": "2.00", "currency": "USD" },
    "ttl": "1h",
    "runner": { "platforms": ["linux"], "memory": "2Gi", "compute": "1000m", "storage": "4Gi" }
  }
}
```

Asking for less than your Envelope is good practice. A run that reads code doesn't need write
tools, and a short run doesn't need your whole budget. The [reference](reference.md#requirements)
lists every field.

## Task

A **Task** is a reusable description of work. Its Task definition
(`steward.task-definition/v2`) names:

- the coding agent (`runtime.agentRef`) and optionally a model;
- the prompt, inline (`promptText`) or as a file next to the definition (`prompt`);
- optional instruction skills;
- the repositories and scratch space to prepare (`workspace`);
- the files it produces (`outputs`);
- optional `requires`.

A Task can live in a repository, usually under `.steward/tasks/<name>/task-definition.json`, or
be saved in the browser. Each Task version is pinned by a digest, so a run always says exactly
which Task it used.

## Run

A **Run** is one execution of a Task. It moves through these stages:

1. **Admission.** Steward checks the request against your Envelope, resolves every repository
   to an exact commit, and records what was approved.
2. **Runtime.** A sandbox is created with only the approved model, tools and budget.
3. **Agent.** The coding agent works on the prompt.
4. **Finalize.** Steward collects the files under `out/`, then removes the sandbox and its
   keys.

A run ends as `succeeded`, `failed` or `cancelled`. See [Read a run](reading-a-run.md).

## Connection

A **Connection** links your own account on another service to Steward, for example GitHub, or
your inference key when the installation uses your own model key. You manage them on the
**Connections** page. A connection lets approved tools act on your behalf. It never widens your
Envelope, and the sandbox never sees the credential.

## Workspace

A run's **workspace** is the set of repositories and scratch directories Steward prepares before
the agent starts, under `/sandbox/workspace`. Repositories are pinned to exact commits and are
read-only unless you ask for a writable copy. Your Envelope must allow workspace access. See
[Check out repositories](repositories-and-workspaces.md).

## Budget

Spending is counted in your Envelope's currency:

- the **monthly limit** covers all your runs under that Envelope in a calendar month;
- the **per-run limit** caps one run.

A run that would start after the monthly limit is reached doesn't start. A run that reaches its
per-run limit stops. Run details show spend and tokens when the model gateway reports them;
otherwise they show **Not reported** (see [Read a run](reading-a-run.md#spend-and-tokens)).
