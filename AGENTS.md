# Agent rules

Mandatory workflow rules for any change to this repository. These override
convenience and personal preference. If a rule is wrong, amend it via PR —
don't bypass it.

**And never amend it yourself.** Editing a rule so that it permits what it
forbade is the one failure this document cannot catch on its own. See §13.

Steward is a governance control plane. Most of the rules below exist because a
shortcut here does not produce a bug, it produces a security property that
quietly stops holding. Prefer a CI check over a paragraph: any rule that can be
mechanised should be, and the ones below that are already mechanised say so.

## 1. Branch → PR → review → merge

No direct pushes to `main`. Every change, no matter how small:

1. **Sync first.** Before creating any branch:
   ```bash
   git fetch origin
   git switch main
   git pull --ff-only
   ```
   If `--ff-only` fails, stop and ask. It means local `main` has diverged, and
   that is never something to resolve unattended.
2. Create a branch. Slice work: `s<N>/<slug>` (e.g. `s3/envelope-admission`).
   Otherwise `feat/<slug>`, `fix/<slug>`, `chore/<slug>`.
3. Push the branch, open a PR against `main`.
4. Keep it current: rebase onto `origin/main` while the branch is yours alone;
   merge `origin/main` in once anyone else has pulled it.
5. CI gates (§7) must be green on the **pinned** lane. The latest-nightly lane
   is informational and does not block.
6. Squash-merge (§6).

### 1.1 Force pushes

**Never force-push.** Not with `--force`, not with `--force-with-lease`, not by
any other route.

The single exception: a human explicitly authorises it **in the current
session**, for a **named branch**. A prior authorisation does not carry
forward, and no authorisation applies to `main` or to any branch someone else
has pulled. When in doubt, add a commit — history that is ugly and true beats
history that is tidy and reconstructed.

### 1.2 Merging and closing are human actions

**Never merge, close, or reopen a pull request.** Not through the web UI, not
through the API, not through `gh`, not as a side effect of any other command.
The same applies to dismissing a review, converting to draft, and deleting a
branch that has an open PR against it.

Opening a PR is agent work. Deciding its fate is not. A closed PR loses review
context that is not recoverable by reopening, and "this one is obsolete" is a
judgement about intent, which is exactly the class of call to hand back.

If a PR looks stale, superseded, or wrong: say so and leave it open.

### 1.3 Pushing — expect a wait, and do not "fix" it

The signing key is held in hardware and gated by biometric confirmation. Two
consequences, and both have tripped agents before:

**A push blocks until a human physically approves it.** That approval has a
timeout. A push that hangs and then fails is almost always *"nobody was at the
keyboard"* — it is not a broken credential, a bad remote, or a permissions
problem.

- **Announce the push before running it**, so the human can be present. Do not
  push silently and then discover the timeout.
- On timeout: say plainly that the push is waiting on approval, and retry once
  when the human confirms they are ready. Do not retry on a loop.

**The agent socket is not always bound in this execution context.** Symptoms:

```
Could not open a connection to your authentication agent
Error connecting to agent: No such file or directory
sign_and_send_pubkey: signing failed for ED25519 ... from agent: agent refused operation
```

Diagnose before retrying:

```bash
ssh-add -l          # lists the key → socket is fine, the problem is elsewhere
                    # errors → the socket is not reachable from here
echo "$SSH_AUTH_SOCK"
```

If the socket is not reachable, **retry exactly once via the documented
escalation below, then stop and report.** Repeated identical attempts are the
failure mode this section exists to prevent — the second attempt fails for the
same reason as the first, and the tenth is just noise in the transcript.

> **Escalation for this environment:** `<TODO — fill in once, from the actual
> setup>`. Deliberately left blank rather than guessed: a wrong command here
> produces exactly the retry loop the rest of this section prevents. Whoever
> hits this first should record what actually worked.

**Never work around a push failure.** Specifically, never:

- switch the remote from SSH to HTTPS,
- introduce a token, PAT, or any credential to make the push succeed,
- pass `-o StrictHostKeyChecking=no` or otherwise relax host verification,
- generate a new key, or add one to the agent,
- disable commit signing.

Every one of these turns an inconvenience into a secret-handling incident. A
blocked push is a blocked push; report it.

### 1.4 Worktrees — inside the repository, or not at all

Start from: **you probably do not need one.** The legitimate cases are narrow —
comparing two revisions side by side, keeping a long run going on one revision
while reading another, bisecting without disturbing the tree.

**Never create a worktree outside the repository root.** Not `/tmp`, not `~`,
not a sibling directory. Anything outside the workspace root falls outside the
approved path, so every subsequent file operation raises an approval prompt.
That is not a permission bug to push through — it is the sandbox doing its job,
and answering the prompts one at a time is the wrong fix. The fix is location:

```bash
git worktree add .worktrees/<slug> <branch>
```

`.worktrees/` is gitignored and inside the approved workspace, so nothing
escalates.

**A worktree is not a route around §6.** If a task needs a different branch,
ask. Do not reach for a worktree because switching branches was declined — the
rule is about not moving the tree out from under a human, and a worktree that
exists to dodge it defeats the same purpose.

**Clean up, on the same terms as §5.**

```bash
git worktree list
git worktree remove .worktrees/<slug>
git worktree prune                     # after any manual deletion
```

Remove it before handing over, or say plainly that you left it and why. A stale
worktree keeps its branch checked out, and that surfaces much later as:

```
fatal: 'feat/x' is already checked out at '.../.worktrees/x'
```

which reads like a branch problem and is not one.

**Worktrees do not inherit untracked files.** No `.env`, no local config, no
`.steward-run/`. **Do not copy them across.** Duplicating `.env` into a worktree
spreads a secret to a second location and a second chance to commit it (§11.1).
Re-derive the config, or do the work in the main tree.

**Do not use worktrees to parallelise the heavy test lane.** §5.6 stands: one
cluster at a time locally. Run IDs keep two runs' artifacts distinct (§5.1);
they do not make the laptop bigger, and two integration lanes competing for it
produce timeouts that look like real failures.

Each worktree gets its own `target/` unless `CARGO_TARGET_DIR` is shared —
and sharing it across differing revisions causes rebuild thrash. Take the disk
cost, or do not use a worktree.

## 2. Strict TDD

For any production code change:

1. **Red** — write the failing test first. Run it. See it fail. The failure
   message must be specific to the behaviour under test, not a compile error or
   a missing import.
2. **Green** — write the minimum code that makes the test pass. No extra
   features, no speculative abstractions.
3. **Refactor** — clean up without changing observable behaviour. The test stays
   green throughout.

Bug fixes start with a failing regression test that reproduces the bug.

**For anything security-relevant, the negative test comes first.** Before the
code that enforces a limit, write the test that attempts to exceed it and
requires failure. Write the escape attempt before the fence. This applies to
admission, the mint, revocation, egress, and every guarantee in the register.

Narrow exceptions: type-only changes, build glue, CI workflows, pure docs,
throwaway diagnostics removed in the same PR.

## 3. The test ladder

| Level | Runs against | When |
|---|---|---|
| **Unit** | nothing external | always |
| **Integration** | kind + the pinned OpenShell, SPIRE, `mcp-gw`, LiteLLM | any change touching `steward-projections`, the controller, or the mint |
| **E2E** | the full stack | every slice |
| **Conformance** | upstream, pinned *and* latest | per release, and at every slice exit |

**Prefer E2E.** A test that drives the real path proves the thing we sell; a
mock proves we can write mocks. Mock only what cannot be stood up in CI, and
record why in the test.

**Every slice exit criterion in the roadmap is a named E2E test.** They are the
definition of done, not a description of it:

```
e2e_s0_provision_and_teardown
e2e_s1_tool_call_as_acting_user
e2e_s2_budget_exhaustion_suspends
e2e_s3_composed_edits_rejected
e2e_s4_grant_binds_to_instance
e2e_s5_terminated_runtime_holds_nothing
```

A slice is not done until its E2E test is green **and** the guarantees it
depends on have been re-run (§10).

## 4. Red is never a steady state

**Never hand over red.** Never open a PR, request review, or call a task done
with a failing test — **including one that was already failing when you
started**. "Pre-existing" is not a defence. You inherit the suite you branched
from. If you cannot fix it, escalate before continuing rather than accumulating
red behind you.

If `main` itself is red when you branch, stop and report before starting any
work. Gates (§7) block merge, so a red `main` means a gate was bypassed or
something moved underneath us. Either is worth interrupting for.

**Never remove or disable a failing test without explicit escalated approval.**
Deletion is the obvious route. These are the others, and they count the same:
`#[ignore]`, commenting out, `#[cfg(...)]`-ing it away, a conditional skip,
widening an assertion until it passes, narrowing the input until it passes.

A test is a claim. Removing one retracts the claim. For a governance product
that is a product decision, not cleanup.

### 4.1 What "escalated approval" means

- **A named human, in writing, in the PR.** Not a passing "sure" in chat, not an
  inference from silence, not a prior approval reused.
- **The reason in the commit body** — why the claim no longer holds, not why the
  test is inconvenient.
- **Its own commit.** A deletion folded into a forty-file diff is invisible,
  which defeats the point of requiring approval at all.
- **For a conformance test, the register is updated in the same commit.**

### 4.2 Resolved is not the same as green

§10 says a red conformance test is a finding, not a chore — do not make it pass.
That does not exempt it from this section. The two compose:

> A red test is either **fixed** or **escalated with a decision recorded**. What
> is forbidden is red *and silent*.

For our own tests, resolved means green. For a conformance test, resolved means
filed against its G-number, the register's status column updated if the
guarantee genuinely changed, and a decision taken — hold the pin, carry a patch,
or amend what we sell. The test may still be red afterwards. It is no longer
unresolved.

### 4.3 Flaky counts as failing

A test that passes on re-run is a failing test that lies, and "just re-run it"
is the most common way this section gets defeated in practice.

Quarantining a flaky test needs the same escalation as removing one, plus a
named owner and an expiry date. An untracked quarantine is a deletion with extra
steps.

## 5. Test infrastructure — ephemeral, labelled, reaped

Integration and conformance tests need real infrastructure: kind or k3s, the
pinned OpenShell, SPIRE, `mcp-gw`, LiteLLM, Postgres. Use local Docker or
Kubernetes when it is available — it is faster than CI and the whole ladder
(§3) is designed to run there.

The discipline below is not tidiness. **Leftover state makes tests lie**, and
the tests most likely to be corrupted by it are the negative ones — a G-test
that "passes" because a previous run left a workspace, a key, or a SPIRE entry
behind is a false green on a security property. That is the exact failure §4
and §10 exist to prevent, arriving through the back door.

### 5.1 Ephemeral by default

```bash
cargo xtask dev up          # cluster + dependencies, unique name, labelled
cargo xtask dev down        # full teardown
cargo xtask dev doctor      # pre-flight: is anything left over?
cargo xtask reap            # delete orphans from earlier runs
```

- **Every run gets its own cluster or namespace**, named with a run ID. Never
  reuse a long-lived dev cluster as a test target.
- **Every artifact is labelled** `steward.test/run-id=<id>` — clusters,
  namespaces, containers, networks, volumes, LiteLLM keys, SPIRE registration
  entries. If it cannot carry a label, it is recorded in the run manifest so the
  reaper can find it.
- **`dev doctor` runs before the suite** and refuses to start on a dirty
  environment. Refusing to start is what prevents the false green; cleaning up
  silently would hide that the previous run leaked.

### 5.2 Teardown is unconditional

Teardown runs on success, on failure, on panic, and on interrupt. It is **not** a
step at the end of a test body — that is precisely the path that does not
execute when the test fails.

In Rust: an RAII guard whose `Drop` performs teardown, created before anything
is provisioned. The test profile stays `panic = "unwind"` so `Drop` runs during
unwind. In shell-driven tasks: a `trap` on `EXIT INT TERM`.

**Credentials created during a test are revoked in teardown, not merely
forgotten.** A LiteLLM key, a SPIRE registration entry, or a signing key that
outlives its test is a real credential, whatever it was created for.

### 5.3 Keeping state for debugging

Legitimate, and never the default:

```bash
cargo xtask dev up --keep      # or STEWARD_DEV_KEEP=1
```

When set, the run prints the exact command to clean up afterwards, and the
artifacts stay labelled so `cargo xtask reap` still finds them. `--keep` is a
debugging session, not a mode of operation — do not set it in CI, and do not
leave it set between tasks.

### 5.4 A manual DEV deployment is opt-in, per session

A manually-maintained DEV environment may exist and may be fully configured in
this repository — a kubeconfig context, an env var, an `xtask` target, a section
in the README.

**None of that is permission to use it.** Availability is a capability, not an
instruction. Never infer authorisation from discoverability.

Use a manual DEV deployment only when a human asks for it **in the current
session**, naming the target. Same shape as §1.1: a prior "yes, deploy to DEV"
does not carry into the next task.

**Never fall back to DEV.** If local Docker or Kubernetes is unavailable, or
`dev up` fails, the answer is to report that and stop. Noticing that a DEV
target happens to be configured and using it because local infra would not start
is the §1.3 failure again — routing around an obstacle into shared
infrastructure — and it is worse here, because DEV plausibly holds real
credentials, real user emails in the `mcp-gw` credential store, and a real
trust domain.

Even when prompted, on a manual DEV:

- **Own a namespace, never `default`**, and never touch a namespace you did not
  create.
- **No destructive tests.** The conformance suite and every teardown,
  revocation, or termination test stays on local ephemeral infrastructure. S5's
  whole job is verifying that termination destroys things; that is not something
  to point at a shared environment.
- **Never run a broad `reap`.** Clean up only what this session created, by run
  ID.
- **Say what you left behind**, where, and under which run ID. Teardown still
  applies unless a human explicitly asks for the deployment to persist.

### 5.5 Never

- **Never run a global prune.** Not `docker system prune -af`, not
  `docker volume prune`, not `kubectl delete ns --all`. These destroy work that
  belongs to the human at that keyboard. Prune **scoped to the run label**, and
  nothing else.
- **Never delete a namespace, cluster, container, or volume you did not
  create.** `reap` operates only on things carrying our label.
- **Never let tests pick up the ambient kube context.** The target comes from an
  explicit variable, and the suite asserts the context name matches the expected
  test pattern before doing anything destructive. A teardown test that runs
  against a shared or staging cluster because `current-context` was inherited is
  the worst outcome in this file.
- **Never point the suite at a shared, remote, or manual DEV cluster** on your
  own initiative — §5.4.
- **Never leave infrastructure running at the end of a task.** If you brought it
  up, take it down before handing over, or say clearly that you left it and why.

### 5.6 Scale to the machine

The heavy lane is serial locally and parallel in CI. Do not start several
clusters at once on a laptop to save wall-clock time — a swapping machine
produces timeouts that look like real failures, which costs far more than it
saved.

## 6. Commits and slices

Branch freely — commit as often as is useful while working. What lands on
`main` is one squashed commit per slice, and its message is the record:

```
S3: envelope admission

Exit criteria:
  - composed-edit sequence rejected (e2e_s3_composed_edits_rejected)
  - identical rejection via webhook and API

Guarantees re-run: G-3 green against <pinned release>
Upstream dependencies recorded: #2109 (watch)
Refs: <issue>
```

Non-slice work squashes to one commit per PR with a conventional-commit subject.

### 6.1 Committing is protection; the branch is the boundary

The rule is about **blast radius**, not about the act of committing. A commit on
your own branch is recoverable and is the only thing standing between hours of
work and a stray `checkout`. Withholding it does not make anything safer.

- **Commit freely on your own branch**, as often as is useful. Uncommitted work
  is unprotected work; a long unattended run that ends with a full slice in the
  working tree has produced nothing durable.
- **Never commit to `main`.** §1.
- **Never switch branches unless explicitly told to.** Stay on the current
  branch for the whole session. If a task appears to need a different branch,
  ask first — and see §1.4 before reaching for a worktree to get around it.
- **Never push without announcing it first.** §1.3.
- **Never merge or close.** §1.2.
The distinction matters most in exactly the case where the old wording failed:
an unattended run. Attended, a withheld commit costs a moment. Unattended, it
risks the whole session.

## 7. Gates

Green before requesting review. Every one of these runs in CI. Run them locally
first — they are the *same* commands, invoked the same way, because task logic
lives in `xtask/` rather than in a CI workflow file.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check                      # licences, advisories, and the §8 layering rule
cargo xtask policy-test               # opa test over policy/
cargo xtask migrate-check             # migrations apply cleanly from empty
cargo xtask verify-manifests          # generated CRD YAML matches steward-types
cargo xtask check-neutrality          # §12 — reserved identifiers only, under test paths
cargo xtask check-secrets             # §11 — scanner over the working tree and the diff
cargo xtask conformance --pinned      # the guarantee suite
cargo xtask register --check          # derived status matches claims; no orphans
```

`cargo xtask ci` runs all of them in order. If it is green, CI will be green;
if CI is green and local is not, that difference is a bug in `xtask` and gets
fixed there rather than papered over in the workflow.

Warnings are not acceptable. `-D warnings` is deliberate: a control plane with a
warning backlog is a control plane where nobody reads the output.

## 8. Architectural invariants — enforced, not remembered

Each of these encodes a decision from the specs. Breaking one is not a style
issue.

1. **One admission library.** Every path that writes desired state goes through
   `steward-admission`. The webhook enforces; the API enforces *and* escalates.
   There is no third door. Never add a write path that skips it, not even for
   tests — use a test fixture that goes through it.
2. **Vendor semantics stay in their adapter.** Only `adapters/<vendor>` may
   depend on that vendor's SDK, name its types, or encode its quirks —
   OpenShell, LiteLLM, `mcp-gw`, Jira, SPIRE, OPA alike. Core crates depend on
   `steward-ports` and nothing below it. *Enforced by `cargo deny`.* If you need
   something a port does not express, widen the port in terms of what Steward
   needs — never by leaking the vendor's shape upward.
   A vendor name belongs in exactly two places: an adapter crate name, and
   configuration. Never in a core type, never in the CRD schema.
3. **Postgres never holds current phase.** Phase lives in CRD `status`, single
   writer, the controller. Postgres is history, queue detail, and observations.
   Status is a cache; if it is stale the next reconcile fixes it.
4. **Join on `runtime_uid`, never on name.** Names are reused. This is not
   hypothetical — it is the upstream workspace hazard we filed.
5. **A grant never edits an envelope.** Approving an over-limit request writes a
   row in `grants` bound to one runtime UID. Anything that mutates `envelopes`
   in an approval path is a bug in the anti-ratchet property.
6. **Spend is observed, never custodied.** LiteLLM is the source of truth.
7. **The mint takes a `Principal`.** No function signature anywhere takes a bare
   acting-user email.
8. **No chat egress in core.** Slack and similar surfaces are reached through
   `NotificationSink`, implemented by a connector — Burble, for Slack. Do not
   add a chat client to any core crate.
9. **No `unwrap()`, `expect()`, or `panic!()` outside tests.** *Enforced by
   clippy.* A control plane that panics has failed open somewhere.

## 9. Ask before

- Adding any dependency.
- Changing the CRD schema, or any field's meaning.
- Modifying a migration that has already been applied — **add a new one
  instead**, never edit history.
- Editing generated files (`manifests/`, `web/src/api-client/`). Regenerate.
- `#[allow(...)]` on a lint. If it is truly needed: a comment saying why, and an
  issue link.
- Anything under `crates/steward-mint/`. It holds the signing key.
- Deploying to, or running anything against, a manual DEV environment (§5.4).
- Force-pushing (§1.1), committing, or switching branches (§6).

Never, with or without asking: merging or closing a PR (§1.2), working around a
push failure (§1.3), rewriting history to scrub a committed secret (§11.3) —
rotate it and report instead — or amending these rules on your own initiative
(§13).

## 10. Upstream discipline

We build on a pre-1.0 dependency. Two rules follow.

1. **Do not infer upstream behaviour — test it.** If what OpenShell, `mcp-gw`,
   or LiteLLM does is uncertain, the answer is a conformance test, not a
   confident comment. "We read the code and it seemed to" is a finding, not a
   fact. Every expensive correction on this project so far came from a
   plausible-sounding inference about a system that was standing still.
2. **No patch in `third_party/` without an upstream attempt recorded beside
   it** — issue, comment, or PR — and a stated exit condition: the upstream item
   whose landing removes the patch.

**A red conformance test is a finding, not a chore.** Never adjust an assertion
to make it pass. Escalate, file it against its G-number, and update the register
if the guarantee's status has genuinely changed.

## 11. Secrets and confidential information

### 11.1 Never commit a secret

**No credential, key, or confidential value is ever committed to this
repository.** Not on a branch, not "temporarily", not commented out, not
base64-encoded, not in a file you intend to remove before opening the PR.

Non-exhaustive, and the last few are the ones that slip through:

- API keys and tokens — provider keys, LiteLLM master or virtual keys, Jira
  tokens, GitHub PATs
- Private keys and certificates — `*.pem`, `*.key`, `*.p12`, the mint's signing
  key, SPIRE trust bundles and agent keys
- Connection strings containing a password
- `.env` and any local override of it
- **Kubeconfigs.** A `kind` kubeconfig embeds client certificates. §5 has tests
  creating clusters, so these are generated routinely — they go to a gitignored
  run path, never into the tree
- **Run artifacts** — anything `cargo xtask dev up` writes
- Customer data of any kind

**The repository holds references, never values.** Config names an environment
variable; it does not contain one. Secrets live in Kubernetes Secrets, the CI
secret store, and a gitignored local `.env` — and nowhere else.

The mint's **public** JWKS is fine and belongs in the tree. Its private half
never touches the filesystem outside a Secret.

### 11.2 Staging discipline

**Never `git add -A`, `git add .`, or `git commit -a`.** Stage explicitly, by
path. Bulk staging is how a stray `.env`, a downloaded kubeconfig, or a test run
artifact enters history — and it enters silently, because nobody reads a
forty-file diff.

Before every commit: read `git status`, then read `git diff --cached`. If a file
appears that you did not deliberately create or modify, stop and ask rather than
staging it.

`.gitignore` covers `.env*`, `*.pem`, `*.key`, `*.p12`, `kubeconfig*`,
`.steward-run/`, and `.worktrees/`, but a `.gitignore` entry is a safety net,
not the rule. The
rule is that you know what you are staging.

### 11.3 If a secret is committed: rotate first

Removing a secret is not remediating it. The moment a credential is committed it
should be treated as compromised — certainly once pushed, and in practice from
the commit itself, since it is on disk, in reflog, and possibly in an editor's
backup or a build cache.

In order:

1. **Rotate or revoke the credential.** Immediately. This is the only step that
   actually restores the security property.
2. **Report it** — plainly, to a human, naming what leaked and where. Do not
   quietly fix it.
3. **Then, and only then**, decide about history.

**Never rewrite history to scrub a secret on your own initiative.** It is a
force-push (§1.1), so it needs explicit human authorisation regardless, and it
is an incident-response decision rather than a cleanup task. A quiet
`commit --amend` is the worst available outcome: the credential is still live,
and now the incident is invisible.

*Gated by `cargo xtask check-secrets`.* A scanner is a backstop for the rule
above, not a substitute for it — it catches shapes it recognises and nothing
else.

### 11.4 Never log secret material

Never log a token, key, SVID, or HOP-1 payload — not at debug level, not in an
error path, not in a test that might run in CI. Types wrapping key material must
not reveal it through `Debug` or `Display` (see `crates/steward-mint/AGENTS.md`).

Test fixtures use values that are obviously fake, and neutral per §12. Never use
a real-shaped credential even if it has been revoked: it trains scanners to
ignore that shape and trains humans to skim past it.

### 11.5 Confidential information is not only credentials

Customer names, contract terms, pricing, internal-only documents, and anything
covered by an NDA do not enter the repository — not in code, comments, `TODO`s,
fixtures, or documentation. §12 makes this structural for tests; this clause
covers everywhere else.

**Commit messages and PR descriptions especially.** They are permanent, they are
harder to scrub than a file, and they are the first thing an external reader
sees if any of this is ever published (D9). Reference an internal ticket by key;
do not paste its contents.

## 12. Test data is neutral

**No ApeLogic, customer, partner, or vendor identity appears anywhere in tests,
fixtures, or testdata.** Not in names, not in emails, not in metadata, not in
comments, not in a `//` aside explaining what the fixture is modelled on.

Three reasons this is a rule and not a preference:

1. **The conformance suite is a publication candidate.** It is leverage in the
   #1613 conversation. Neutral from day one means publishing is a push; anything
   else means a legal and comms review first, which in practice means never.
2. **Anything we upstream should not read as carved out of an internal
   codebase.** Vendor-flavoured test code lowers the odds of a merge and invites
   the "this is vendor-specific" objection on the merits of its naming alone.
3. **Email is the join key of the entire system.** Fixtures are exactly where
   real addresses accumulate naturally, and a fixture carrying a real address is
   both a disclosure and a coupling to a live directory.

### 12.1 Use the reserved ranges

They exist for this. Use them and there is nothing to decide per-test:

| Thing | Use | Never |
|---|---|---|
| Email | `alice@example.com`, `bob@example.org` (RFC 2606) | any real or plausible address |
| People | `alice`, `bob`, `carol`, `dave` | colleagues, customers, anyone real |
| Hostnames | `*.test` (RFC 6761), `*.example.com` | internal hostnames, real clusters |
| IP literals | any address that is not globally routable — loopback, unspecified, link-local, private, documentation | anything globally routable |
| Roles | `engineer`, `analyst`, `admin` | real internal role names |
| Teams / orgs | `team-a`, `acme`, `example-org` | real teams, real customers |
| Issue keys | `PROJ-123` | real Jira keys |
| Clusters, namespaces | `steward-test`, `default` | real cluster or namespace names |

### 12.2 What is *not* covered

Naming a **dependency under test** is required, not a violation. OpenShell,
`mcp-gw`, LiteLLM, SPIRE, and OPA are the systems being exercised; a conformance
test cannot describe what it asserts without naming them. Real upstream issue
numbers in comments are public and welcome.

The line is between a **technical dependency** — name it — and a **commercial
relationship or an internal identity** — never.

### 12.3 Enforced by allow-list, not deny-list

*Gated by `cargo xtask check-neutrality`.* The check asserts that every email,
hostname, and IP literal under test paths **matches a reserved pattern**. It
does not scan for a list of forbidden names.

That direction is deliberate. A deny-list of customer and partner names is
itself a disclosure artifact — a file enumerating exactly who we work with,
committed to the repository it is meant to protect. An allow-list on identifier
*shape* stores nothing sensitive, and catches names nobody thought to add.

Anything intended for upstream — a PR, a published suite, an issue reproducer —
follows this section whether or not it lives under a test path.

## 13. Amending these rules

This document is not working material. It is the standing agreement about how
work happens here — and an agent that can edit it can authorise itself.

**Never modify `AGENTS.md`, `CLAUDE.md`, or any nested `AGENTS.md` unless a
human explicitly asks.** Noticing that a rule is wrong, stale, or
self-contradictory is genuinely useful — say so, in the response or the PR
body. Proposing is the job. Enacting is not.

### 13.1 The change that must never happen

**Never edit a rule in the same change the rule would have blocked.** Concretely:

- blocked from force-pushing → editing §1.1
- a test is red → editing §4
- a secret was committed → editing §11
- a conformance assertion will not pass → editing `conformance/AGENTS.md`

Each of these is self-authorisation. It is worse than the underlying violation,
because the violation is one event and this removes the tripwire for everyone
who comes after.

### 13.2 When an amendment is asked for

- **Its own PR, containing nothing else.** A rule change inside a work PR is
  invisible for exactly the reason a deleted test is (§4.1) and a bulk-staged
  file is (§11.2) — nobody reads a forty-file diff.
- **The body says what became false**, not what became inconvenient.
- **Weakening or removing a rule takes the §4.1 escalation shape.** A rule is a
  claim about how we work, as a test is a claim about the system. Removing one
  retracts the claim, and that is a decision with an author.
- Adding a rule is cheaper, and still human-initiated.

### 13.3 The enforcement surface counts as rules

A rule enforced by configuration is only as strong as the configuration. These
are covered by this section exactly as the prose is:

- `deny.toml` — the §8 layering rule and the advisory set
- lint configuration, and any `#[allow]` that disables a §8 lint
- CI workflow files, and the `xtask` implementations behind `check-neutrality`,
  `check-secrets`, `register --check`, and `verify-manifests`
- the security entries in `.gitignore` (§11.2)
- `claim` values in `conformance/register.toml` — evidence cannot raise a claim,
  and neither can an agent
- branch protection settings

**Weakening a check is a rule change wearing a config file.** If a gate is
failing, the gate is the message.

### 13.4 What is not an amendment

Correcting a factual reference — a renamed command, a broken section link, a
path that moved — is maintenance. Still call it out, and still keep it out of a
work commit.

If you cannot tell which one it is, it is an amendment.

## 14. How the agent applies this

- Never push to `main`. If asked to "push", check out a branch first.
- Never write a function before the test that calls it.
- Never force-push, commit, or switch branches on your own initiative.
- Never merge or close a PR. Open it, say what you think, hand it over.
- Announce a push before running it, and treat a hang as a human waiting to
  approve it rather than as something to diagnose.
- If a rule conflicts with a user instruction, ask before bypassing.
- If you are about to weaken a test to make something pass, stop and say so
  instead.
- Never edit these rules. If one is wrong, say so and keep following it (§13).
- Never say "that test was already failing." You inherit it (§4).
- Never delete, `#[ignore]`, or skip a red test on your own judgement.
- Never leave a cluster, container, or volume running at the end of a task.
- Never run a global prune. Ever. Scope it to the run label (§5.5).
- Never `git add -A` or `git add .`. Stage by path, and read what you staged.
- Never create a worktree in `/tmp` or anywhere outside the repository. Use
  `.worktrees/`, and remove it when done (§1.4).
- If you committed a secret: say so and rotate it. Do not amend it away.
- Never use a manual DEV deployment because it happens to be configured. Wait to
  be asked, every time (§5.4).
- Never reach for a real name, address, team, or customer when writing a
  fixture. `alice@example.com` is always available and always correct.

---

Package-specific rules live alongside each crate (e.g.
`crates/steward-mint/AGENTS.md`, `conformance/AGENTS.md`). The nearest file
composes with this one; it never overrides §1, §2, §4, §8, or §13.
