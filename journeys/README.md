# User journeys as feature files

Ten journeys for the web UI (brand name in the UI: HyperShell), one file each. A journey is one person, one goal, from start to finish. The files are plain text that a person can read and a test runner can run.

The feature set comes from the user-journey specification (J1 to J10). The `@smoke` scenarios in
J1 through J4 are executable; later scenarios retain their source annotations until their step
implementations are added.

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

1. A new feature first goes into the journey whose goal it serves, as a new scenario. Start a new file only for a new goal. Name it `J11-short-name.feature`.
2. Write each line with a pattern from `STEPS.md`.
3. If no pattern fits, write the sentence the way a person would say it, and add it to `STEPS.md` in the same change. See "A sentence with no code behind it" below.
4. Add the tags that apply.

## Writing rules

- **One person, one goal.** "I" is the person of the journey. A second person is named by role: "an administrator", "a user", "the operator".
- **One account by default.** A journey uses one universal account: a user who is also an administrator (`Given I am signed in as a user who is also an administrator`), or a user with one role. That person asks as a user, switches to the administrator area of the same UI, and decides. Use a second account only when the scenario is about separation of roles (what a person without a role must not see or do) or about another person's data, and tag it `@two-accounts`.
- **Only what a person does and sees.** No page addresses, no element names, no server answers, no stored records, no internal words.
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
| `no-agent` | Template whose only model works with no configured agent (added for J8; not in the specification). |
| `<owner>/<repository>` | A repository the installation allows. `<owner>/<other-repository>` is one it does not allow. |
| `<Agent A>`, `<Agent B>` | The two coding agents, by the name the UI shows. `<agent-b>` is the short name. |
| `<provider-a>/<model-a>`, `<provider-b>/<model-b>` | The models that work with agent A and agent B. |
| `<provider-a>/<new-model>` | A model added in configuration only (J10). |
| `<provider-c>/<model-c>` | A model no configured agent can use (J10). |
| `<run-runtime-minutes>` | A within-template runtime allowance derived from the unique smoke run prefix. |
| "member" | The role a template is meant for. |

Limits such as 2 GiB, 1,000 generations or 100.00 USD are the values of the test installation.

## Tags

| Tag | Meaning |
|---|---|
| `@smoke` | Part of the smallest set that proves a release is usable: J1 (steps 1 to 9), J2, J3 (approve path), J4 (steps 1 to 6). Run in this order with one unique run prefix. |
| `@needs-model` | Makes a real model call. Costs money. |
| `@needs-github` | Needs an external repository that the installation allows. |
| `@two-accounts` | Needs a second signed-in person. |
| `@operator` | Needs a change to the installation outside the UI. |
| `@nightly` | Long. Not for every build. |
| `@known-failing-*` | Fails for the version or public issue named by the suffix. The note above the tag names the finding or case. Remove the tag when the scenario passes. |

A tag on the `Feature` line applies to every scenario in the file.

## A sentence with no code behind it

A sentence runs only when a small piece of test code exists for its pattern. The `@smoke` scenarios
in J1 through J4 have step code; the remaining scenarios are specifications until their journey
implementation lands.

When a file holds a sentence that has no code:

- the runner does not guess. It reports the sentence as "undefined" (or "missing step") and names the file and line;
- the scenario is not counted as passed;
- the report shows a starting point for the missing code, which a test engineer completes.

So a maintainer can write a new sentence at any time. The cost is one request to a test engineer, and the scenario stays unproven until it is answered. Reusing a pattern from `STEPS.md` costs nothing.

The runner includes J1 through J4. The remaining feature specifications do not look tested before
their step code lands. Within the selected scenarios, a missing sentence fails generation and no
browser scenario starts.

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
precondition: an existing personal connection does not change these journeys. When
`STEWARD_JOURNEY_AGENT` or `STEWARD_JOURNEY_MODEL` is set, the matching enabled option is selected;
otherwise the forms' defaults are kept. The run prefix must be unique for the target installation;
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
