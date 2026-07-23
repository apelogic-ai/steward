# Generating the guarantee register from test outcomes

Status: design — implement in S0.0 alongside the suite itself
Companions: *Steward Roadmap* §8, `conformance/AGENTS.md`, `conformance/register.toml`

> **Problem.** §8.1 of the roadmap is a table with a status column. A
> hand-maintained status column drifts, and the two invariants that rest on it —
> *nothing is sold that the register does not mark provided*, and *nothing is
> marked provided without a green negative test* — are unenforceable while a
> human types the word "provided".
>
> **Fix.** Split the register. Prose is authored; **status is derived from the
> test run**. A generator joins them and refuses to render a claim the evidence
> does not support.

---

## 1. The split

| Half | Lives in | Authored by | Can it lie? |
|---|---|---|---|
| What we sell, mechanism, watch item, `claim` ceiling | `conformance/register.toml` | a human, deliberately | yes — but only downward, see §4 |
| Status, per-lane outcomes, evidence list | the test run | nobody | no |

`register.toml` deliberately has **no status field.** It has `claim`, which is
the *strongest status this entry is permitted to reach*. Evidence may fall short
of the claim — that is a finding. Evidence can never push a claim upward on its
own; raising a claim is an edit to the file, which is a decision with an author
and a diff.

## 2. Discovery is by convention

No proc macro, no `inventory`, no custom harness. One module per guarantee:

```
conformance/tests/
  g1_egress.rs
  g2_credential_isolation.rs
  g3_policy_propagation.rs
  g4_revocation.rs
  g5_model_allowlist.rs
  g6_cross_agent_isolation.rs
```

A test can only live in one file, which is how `conformance/AGENTS.md`'s "every
test maps to exactly one register entry" becomes structurally true rather than a
rule people remember.

Within a module, the function prefix says what the test asserts:

| Prefix | Asserts | Green means | Red means |
|---|---|---|---|
| `holds_` | the violation fails | the guarantee holds | **regression** — we may be selling something untrue |
| `gap_` | the violation still succeeds | the documented gap is unchanged | **the gap may have closed** — upstream improved; the register is stale in our favour |

The `gap_` direction is the part worth having. A register that only detects
regression tells you when the foundation crumbles. One that also detects
*solidification* tells you when a limit you have been apologising for has
quietly become real — which is when to raise the claim and stop under-selling.

> **Why a naming convention rather than `#[guarantee(G-2)]`.** The attribute
> needs a proc-macro crate, a build-time cost, and either `linkme`-style runtime
> registration or a `syn` pass to extract. The convention needs neither, and
> the §5 bidirectional check makes typos loud rather than silent. If per-test
> metadata is ever needed beyond the G-number, moving to an attribute is
> additive — only the extraction step changes; the join, the derivation, and
> the checks are unaffected.

## 3. Input

`cargo nextest run -p conformance --message-format libtest-json` gives, per
test: full path (`g2_credential_isolation::holds_forged_svid_rejected`),
outcome, and duration. Both lanes run:

- **pinned** — against the release in `[meta] pinned_openshell`. Gates.
- **latest** — against upstream `latest`. Informational, and the early-warning
  signal §8.2 already calls for. Surfacing it *in the register* rather than only
  in CI output is most of the value: a latest-lane red on a `holds_` test is
  upstream about to break something we sell.

## 4. Derivation

```rust
enum Derived {
    Provided,          // all holds_ green, no gap_
    Partial,           // holds_ green, but register declares open gaps
    NotYetProvided,    // only gap_ tests, all green
    Regressed,         // any holds_ red
    GapMayHaveClosed,  // any gap_ red
    Unevidenced,       // no tests at all
}

fn derive(entry: &Entry, results: &[TestResult]) -> Derived {
    let (holds, gaps): (Vec<_>, Vec<_>) =
        results.iter().partition(|r| r.name.starts_with("holds_"));

    if holds.is_empty() && gaps.is_empty()      { return Derived::Unevidenced; }
    if holds.iter().any(|r| r.failed())         { return Derived::Regressed; }
    if gaps.iter().any(|r| r.failed())          { return Derived::GapMayHaveClosed; }
    if holds.is_empty()                         { return Derived::NotYetProvided; }
    if !entry.gaps.is_empty()                   { return Derived::Partial; }
    Derived::Provided
}
```

Then the rule that makes the whole thing worth building:

```rust
// A claim may exceed what the evidence shows — that is a finding, reported.
// Evidence may never silently raise a claim.
let published = min(entry.claim, derived);
```

`Regressed` and `GapMayHaveClosed` are never `published`; they render as
themselves, loudly, because both are findings under root `AGENTS.md` §4.2 —
resolved by escalation and a recorded decision, not by editing this file.

## 5. Checks — bidirectional, both fatal

`cargo xtask register --check` fails on any of:

| Condition | Meaning |
|---|---|
| A guarantee with `claim ≠ planned` and no tests | **An unevidenced claim.** We are describing something we do not test |
| A `g<N>_*` module with no matching entry | **Orphan evidence.** A typo silently drops a test's testimony |
| `claim = "planned"` with any tests | The entry outgrew its status; promote it deliberately |
| `derived < claim` on the **pinned** lane | We are claiming more than we can show |
| Any `Regressed` on the pinned lane | Guarantee broke |
| The rendered block in the roadmap differs from freshly generated output | Someone hand-edited generated content |

The latest lane never fails the check. It is reported in the table and alerted
on — making it blocking would trade an early-warning signal for a stoppage,
which `conformance/AGENTS.md` §6 already refuses.

## 6. Output

Rendered between markers, the same splice convention `observer` already uses for
nested agent rules:

```markdown
<!-- BEGIN:generated-register -->
...table...
<!-- END:generated-register -->
```

Targets: roadmap §8.1, and a standalone `docs/guarantee-register.md` for anyone
who wants the artifact without the roadmap around it.

Columns: `#` · Guarantee · Mechanism · **Status (derived)** · Pinned · Latest ·
Evidence (n tests) · Watch.

**Provenance line, always rendered:** pinned release, commit SHA, run timestamp,
lane results. Without it the table is an assertion; with it, it is a record — and
this eventually leaves the building (D9).

## 7. Commands

```bash
cargo xtask register             # run both lanes, render, write
cargo xtask register --check     # render to memory, diff, exit non-zero on §5
cargo xtask register --lane pinned
cargo xtask register --render    # re-render from the last run, no tests
```

`--check` joins the §7 gate list. `register` proper runs in the per-release CI
job (§8.2) and on every slice exit (§8.3).

## 8. What this buys

1. **The status column cannot drift.** Invariant 10 stops being a rule people
   remember and becomes a build failure.
2. **Unevidenced claims are impossible to keep.** Adding a row to the register
   without a test breaks the build — the cheapest possible moment to notice that
   we are describing something we have not verified.
3. **Solidification is detected, not just regression.** `gap_` tests turn "we
   under-sell G-6" into a red test with a name.
4. **D9 gets cheaper.** The suite is already neutral (§12) and the register is
   already generated with provenance. Publishing becomes a render, not a
   project.
5. **The Gherkin question mostly dissolves.** The artifact a non-engineer wanted
   — an auditable, executable specification of what the system guarantees —
   exists, without a step-definition layer to maintain.

## 9. Open

| # | Question | Position |
|---|---|---|
| GR1 | Do slice E2E tests join the register too? | No. They prove *our* behaviour; the register is about the foundation. Keep the boundary sharp or the register becomes a test index |
| GR2 | Sub-guarantee granularity (`G-2.1`, `G-2.2`) | Not yet. `gaps[]` prose carries it until a real case forces the split |
| GR3 | Per-test human-readable assertion strings | Deferred. This is the case that would justify moving to `#[guarantee(...)]`; revisit if the rendered table reads poorly |
| GR4 | Machine-readable export (JSON) alongside markdown | Yes, cheap — do it when the first consumer appears, not before |
