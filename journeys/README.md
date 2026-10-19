# User journeys as feature files

Thirteen journeys cover the web UI (brand name in the UI: HyperShell) and service callers, one file
each. A journey is one person or caller, one goal, from start to finish. The files are plain text
that a person can read and a test runner can run.

The feature set comes from the user-journey specification (J1 to J10), plus the Jira acceptance
journey J11, the service-caller acceptance journey J12, and the personal-agent acceptance journey
J13. The runner loads all thirteen
files. `@smoke` executes the release-critical paths in J1 through J4; `@extended` executes the
currently passing one-account paths in J2 and J6 through J8. J5's documented paths still carry
product findings, J9 needs a second signed-in person, and J10 needs operator control of the
installation, so those paths remain specifications until those prerequisites and expected outcomes
are available.

| File | Journey |
|---|---|
| `J01-…` | Administrator sets up a new installation |
| `J02-…` | New user gets going |
| `J03-…` | User needs more than the template allows |
| `J04-…` | User runs a Task on a repository |
| `J05-…` | User runs a Task with scratch space |
| `J06-…` | User asks for more than their Envelope allows in a Task |
| `J07-…` | User watches and stops a run |
| `J08-…` | User chooses agent and model |
| `J09-…` | Administrator oversight |
| `J10-…` | Operator changes configuration |
| `J11-…` | User runs a Task with Jira |
| `J12-…` | Jenkins, Temporal and Jira callers run governed work |
| `J13-…` | User creates and chats with a personal agent |

`STEPS.md` lists every sentence pattern the files use.

## How a file is built

- **Feature**: the journey, with the person and the goal.
- **Background**: what must be true before the person starts. It always says which optional integrations are switched off.
- **Scenario**: one path through the journey. The first scenario is the main path. Each failure path or variant has its own scenario.
- **Scenario Outline** with **Examples**: the same path tried with several values. One table row is one try.
- Lines start with `Given` (what is true before), `When` (what the person does), `Then` (what the person must see). `And` repeats the line before.
- A line that starts with `#` is a note for the reader. It is not run.

## To edit a journey

1. Open the file. Find the scenario.
2. Change the sentence. Keep its pattern (see `STEPS.md`); change only the text in quotes or the number.
3. If the UI text changed, copy the new text exactly, with its punctuation.

## To add a scenario or a journey

1. A new feature first goes into the journey whose goal it serves, as a new scenario. Start a new file only for a new goal. Name it `J14-short-name.feature`.
2. Write each line with a pattern from `STEPS.md`.
3. If no pattern fits, write the sentence the way a person would say it, and add it to `STEPS.md` in the same change. See "A sentence with no code behind it" below.
4. Add the tags that apply.

## Writing rules

- **One person or caller, one goal.** "I" is the person of a UI journey. A second person is named by role: "an administrator", "a user", "the operator". A caller journey names the external system that acts.
- **One account by default.** A journey uses one universal account: a user who is also an administrator (`Given I am signed in as a user who is also an administrator`), or a user with one role. That person asks as a user, switches to the administrator area of the same UI, and decides. Use a second account only when the scenario is about separation of roles (what a person without a role must not see or do) or about another person's data, and tag it `@two-accounts`.
- **Only observable behavior.** UI journeys say what a person does and sees. Caller journeys say what the caller sends and receives. Neither exposes implementation details or stored secrets.
- **Exact UI text in quotes.** If the text is not known yet, write a plain sentence without quotes and mark it with a note `# UI text to confirm`.
- **State the expected text.** Where the product today shows something else, the file still holds the text a person should see. Tag the scenario `@known-failing-<version>`.
- **A unit on every number.** "15 min", "10.00 USD", "2 GiB", "0 new entries". A value typed into a field is written exactly as typed; the unit is in a note above the line or in the column name.
- **Every failure path ends in a message the person can act on** and creates nothing: finish with `the page "…" lists 0 new entries`.
- **Neutral data.** Use the placeholders below. No real names, hosts, addresses or identifiers.

## Placeholders

| Placeholder | Meaning |
|---|---|
| `standard` | Template: Git and Scratch workspaces, no runtime limit. Example limits: 10.00 USD per month, 1.00 USD per run, 15 min. |
| `standard-1g` | As `standard`, with a total size limit of 1 GiB. |
| `git-only` | Template: Git workspaces only. |
| `timed` | Template: runtime limit of 60 min per month, no workspace types. |
| `basic` | Template: no workspace access, one allowed model, no runtime limit. |
| `broad` | Template: models from two providers. |
| `jira-read` | Template: exactly one Jira read tool and no workspace access. |
| `no-agent` | Template whose only model works with no configured agent (added for J8; not in the specification). |
| `<owner>/<repository>` | A repository the installation allows. `<owner>/<other-repository>` is one it does not allow. |
| `<Agent A>`, `<Agent B>` | The two coding agents, by the name the UI shows. `<agent-b>` is the short name. |
| `<provider-a>/<model-a>`, `<provider-b>/<model-b>` | The models that work with agent A and agent B. |
| `<provider-a>/<new-model>` | A model added in configuration only (J10). |
| `<provider-c>/<model-c>` | A model no configured agent can use (J10). |
| `<run-runtime-minutes>` | A within-template runtime allowance derived from the unique smoke run prefix. |
| `<run-prefix>` | The unique `STEWARD_JOURNEY_RUN_PREFIX`, also used in caller idempotency identities and Jira payloads. |
| "member" | The role a template is meant for. |

Limits such as 2 GiB, 1,000 generations or 100.00 USD are the values of the test installation.

## Tags

| Tag | Meaning |
|---|---|
| `@smoke` | Part of the smallest set that proves a release is usable: J1 (steps 1 to 9), J2, J3 (approve path), J4 (steps 1 to 6). Run in this order with one unique run prefix. |
| `@extended` | One-account coverage beyond smoke: J2's onboarding dismissal preference, J6's accepted requirements, J7's terminal captured/no-capture logs, and J8's agent/model selection and Task persistence. Run after `@smoke` with the same prefix. |
| `@needs-model` | Makes a real model call. Costs money. |
| `@needs-github` | Needs an external repository that the installation allows. |
| `@needs-jira` | Needs a connected Jira account and the `jira-read` template fixture created after `@smoke` with the same run prefix. |
| `@callers` | Exercises external caller exchange, Run API and receiver paths in J12. |
| `@personal-agents` | Exercises the signed-in personal-agent path in J13. It creates its own standard-template Envelope fixture. |
| `@covered-by-process-e2e` | Covered by the named real-process integration test instead of an installation journey. Unlike `@known-failing-*`, it is not waiting for a product fix. |
| `@live-jira-webhook` | Causes a real Jira event to reach a public receiver. It also requires `@owner-only`. |
| `@needs-public-ingress` | Requires the target receiver to be reachable from the external service. |
| `@owner-only` | Runs only after the owner explicitly authorizes that live external action for the current run. |
| `@two-accounts` | Needs a second signed-in person. |
| `@operator` | Needs a change to the installation outside the UI. |
| `@nightly` | Long. Not for every build. |
| `@known-failing-*` | Fails for the version or public issue named by the suffix. The note above the tag names the finding or case. Remove the tag when the scenario passes. |

A tag on the `Feature` line applies to every scenario in the file.

## A sentence with no code behind it

A sentence runs only when a small piece of test code exists for its pattern. The `@smoke` and
`@extended` scenarios have step code; other scenarios remain specifications until their journey
implementation lands.

When a file holds a sentence that has no code:

- the runner does not guess. It reports the sentence as "undefined" (or "missing step") and names the file and line;
- the scenario is not counted as passed;
- the report shows a starting point for the missing code, which a test engineer completes.

So a maintainer can write a new sentence at any time. The cost is one request to a test engineer, and the scenario stays unproven until it is answered. Reusing a pattern from `STEPS.md` costs nothing.

The runner includes all thirteen files, but tags select only the intended scenarios. Feature
specifications without step code do not look tested: selecting one fails generation before a
browser starts.

J6's accepted-requirements scenario currently runs in the known-failing lane for issue #414 only
when it states a per-run budget of zero and the run either fails or remains running at its deadline
with no failure reason. The other selected extended scenarios remain required.

## Run against an installation

These journeys use the real web application and its real API. They do not start the browser mock
server from `tests/browser` and do not intercept routes.

Sign in once by hand and save Playwright storage state outside the repository. For example, launch
an interactive browser with a neutral placeholder URL:

```console
bunx playwright codegen --save-storage=/path/to/steward-storage-state.json https://steward.example.com
```

Complete sign-in in that browser, then close it. Run the smoke set with environment-specific values
supplied at execution time:

```console
export STEWARD_JOURNEY_BASE_URL=https://steward.example.com
export STEWARD_JOURNEY_STORAGE_STATE=/path/to/steward-storage-state.json
export STEWARD_JOURNEY_REPOSITORY=example-org/example-repository
export STEWARD_JOURNEY_MEMBER_ROLE=engineer
export STEWARD_JOURNEY_RUN_PREFIX=journey-001
# Optional: select these instead of the Run and Task forms' defaults.
export STEWARD_JOURNEY_AGENT=agent-ref-or-display-name
export STEWARD_JOURNEY_MODEL=provider/model
cargo xtask journeys --tags @smoke
```

Run the additional paths with the same prefix after smoke has created the `standard` and `timed`
templates and their Envelopes. The extended set is not standalone; it depends on those smoke-created
objects under that prefix:

```console
cargo xtask journeys --tags @extended
```

Run the personal-agent path with its own unique prefix. It creates the standard template and its
active Envelope, then creates a run-prefixed personal agent, sends one message, waits for the
reply, and opens the linked Run:

```console
cargo xtask journeys --tags @personal-agents
```

After the owner connects Jira, run its acceptance journey with the same prefix that created the
`standard` template during smoke:

```console
cargo xtask journeys --tags @needs-jira
```

### Service callers

J12 calls the exchange, Run API and receiver endpoints directly. It uses a short-lived
administrator bearer to prove that a refusal or replay created no extra Task; it does not need a
signed-in browser storage state. Its administrator HTTP calls stay outside Playwright. Configure
these non-secret values for the target installation:

| Variable | Value |
|---|---|
| `STEWARD_JOURNEY_ADMIN_TOKEN_FILE` | Absolute path outside the repository to a mode-`0600` file containing one short-lived Steward administrator bearer. The runner resolves symlinks, reads the file again for every administrator request and never sends it through Playwright. |
| `STEWARD_JOURNEY_JENKINS_EXCHANGE_URL` | Exact generic OIDC exchange URL ending in `/v1/oidc/exchange`. |
| `STEWARD_JOURNEY_JENKINS_WORKFLOW` | Versioned Workflow bound to the Jenkins service, such as `release-summary@1`. |
| `STEWARD_JOURNEY_TEMPORAL_EXCHANGE_URL` | Exact workload exchange URL ending in `/v1/workload/exchange`. |
| `STEWARD_JOURNEY_TEMPORAL_WORKFLOW` | Versioned Workflow bound to the Temporal service. |
| `STEWARD_JOURNEY_UNBOUND_EXCHANGE_URL` | Exchange URL that will receive the deliberately unbound assertion. |
| `STEWARD_JOURNEY_JIRA_RECEIVER_ORIGIN` | Origin of the receiver whose Trigger uses a read-only Envelope. |
| `STEWARD_JOURNEY_JIRA_TRIGGER_ID` | That read-only Trigger's configured ID. The local-main acceptance configuration uses `local-read`. |
| `STEWARD_JOURNEY_JIRA_COOLDOWN_SECONDS` | The read-only Trigger's configured per-issue cooldown in seconds; J12 requires at least `60`. |
| `STEWARD_JOURNEY_JIRA_MAX_ISSUE_COOLDOWNS` | The receiver's configured issue-reservation capacity; J12 requires at least `4`. |

Credential helpers are absolute executable paths outside the repository. Each helper receives no
arguments and must write only one fresh source assertion to standard output. The runner invokes it
again for every exchange and never reports its output. Configure
`STEWARD_JOURNEY_JENKINS_SOURCE_HELPER`, `STEWARD_JOURNEY_TEMPORAL_SOURCE_HELPER`, and
`STEWARD_JOURNEY_UNBOUND_SOURCE_HELPER`. Put the Jira signing secret in a credential file outside
the repository and set `STEWARD_JOURNEY_JIRA_SIGNING_SECRET_FILE` to its absolute path.

For `@callers`, `@callers and not @live-jira-webhook`, and the two selectors shown below, the runner
does not require `STEWARD_JOURNEY_STORAGE_STATE`, `STEWARD_JOURNEY_REPOSITORY`, or
`STEWARD_JOURNEY_MEMBER_ROLE`. Other journey selections still require all three browser inputs.

The read-only Trigger must have a per-issue cooldown of at least 60 seconds and the receiver must
retain at least four issue reservations. Those bounds keep the second same-issue delivery inside
the cooldown while leaving room for both earlier accepted deliveries and the optional live case.

The write-capable refusal stays out of local-main because constructing its deliberately unsafe
receiver there would weaken the shared test installation. The real-process integration test at
`bins/steward-trigger-receiver/tests/write_catalog_process.rs` covers both enforcement points: a
running receiver refuses the next signed delivery after a catalog grant becomes write-capable, and
a fresh receiver refuses to start with that unsafe catalog. Both paths use the real receiver and
TLS-required PostgreSQL and assert that no Task is created.

Once apelogic-ai/gitops#272 lands, local-main uses the `local-read` Trigger and its exact edge route
for `STEWARD_JOURNEY_JIRA_RECEIVER_ORIGIN`. All remaining direct signed-payload and refusal paths
run with one command:

```console
cargo xtask journeys --tags '@callers and not @live-jira-webhook and not @covered-by-process-e2e'
```

The direct receiver assertions follow its public response contract: `202` means one Task was
accepted, `200` means a replay or filtered event needed no run, `401` means the signature was
missing or invalid, and `429` means the Trigger's concurrency or cooldown limit refused the
delivery. The process integration test asserts the write-capable Envelope's exact `400` read-only
refusal separately.

The live Jira path additionally requires a public receiver, an owner instruction for the current
run, `STEWARD_JOURNEY_ALLOW_LIVE_JIRA_WEBHOOK=$STEWARD_JOURNEY_RUN_PREFIX`, the exact neutral issue key in
`STEWARD_JOURNEY_LIVE_JIRA_ISSUE_KEY`, and an absolute no-argument
`STEWARD_JOURNEY_LIVE_JIRA_HELPER`. The helper performs the already-configured Jira action and its
output is discarded. With those safeguards in place, the installation caller set remains one
command; the real-process integration test continues to cover write-capable refusal separately:

```console
cargo xtask journeys --tags '@callers and not @covered-by-process-e2e'
```

The extended fixture authors run-prefixed `basic` and `broad` templates through the real admin API
when they do not exist, then requests their within-template Envelopes through the real user API.
Those objects remain on the target installation, just like the smoke templates and Envelopes.

For an installation that intentionally uses a private or locally trusted HTTPS certificate, opt in
to Playwright's certificate bypass for that run:

```console
export STEWARD_JOURNEY_IGNORE_HTTPS_ERRORS=true
```

The variable accepts only `true` or `false` and defaults to `false`.

The storage-state file is a credential-bearing local artifact: never put it in this repository.
Trim it to the installation origin before use. Failure traces and `error-context.md` files under
`target/journeys/results` can contain the signed-in person's name, email address, and session
cookies; handle and remove them as credential-bearing local artifacts.
The runner reports an explicit refresh instruction if the session has expired. J4 requires a
public GitHub repository admitted by the installation, with `main` as its default branch and a root
file whose name contains `README`; the journey verifies the resolved commit against GitHub's branch
head. The member role must be one held by the signed-in test account and replaces the `member`
placeholder in journey text. The account must also have administrator access because J1 and J3
use administrator APIs and screens. Tool-connection state is recorded but is not an account
precondition: an existing personal connection does not change these journeys. The account must
have incomplete onboarding progress so J2 can exercise the visible Get started guide; the runner
temporarily reopens a dismissed guide and restores the account's original dismissal preference.
A configured
`STEWARD_JOURNEY_AGENT` or `STEWARD_JOURNEY_MODEL` must exist in the installation and be allowed by
the selected Envelope. The deliberately narrowed `basic` Envelope is the only exception: it keeps
the form's compatible default when the configured runtime is unavailable. Without either setting,
the forms' defaults are kept. The run prefix must be unique for the target installation;
every object created by the journeys includes it. J2 and J3 derive a distinct, within-template
runtime-minutes candidate from that prefix and advance to an unused active-Envelope value when
needed, so prior active Envelopes do not turn a new request into a duplicate.

The runner first validates that every selected sentence has step code. It then reports ordinary
scenarios as the required result and runs scenarios tagged `@known-failing-*` separately. It reports
their count and treats only each tag's documented product defect as an expected failure; setup,
authentication, selector, and infrastructure failures remain fatal. An unexpected pass also makes
the run fail so the obsolete tag is removed deliberately. A generation error, including an
undefined sentence in either group, is always fatal. A tag expression that selects no scenario is
also fatal.
