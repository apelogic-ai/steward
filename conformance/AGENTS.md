<!-- BEGIN:conformance-agent-rules -->
# These tests assert someone else's behaviour

Everything here tests **upstream** — OpenShell, `mcp-gw`, LiteLLM — not us. That
inverts the usual instinct about a failing test.

**A red test here is a finding about the foundation, not a task to fix.**

1. **Never weaken an assertion to go green.** Not by loosening a matcher, not by
   adding a skip, not by narrowing the input. If a test that passed now fails,
   upstream behaviour changed, and that is the most valuable signal this
   repository produces.
   Root §4 applies here with one adaptation: a red test in this directory is
   *resolved* by a filed finding and a recorded decision, not by becoming green.
   Resolved and red is fine. Red and silent is not.
2. **On red:** file it against its G-number, update the register's status column
   if the guarantee genuinely changed, and escalate. Then decide — hold the pin,
   carry a patch (with an upstream attempt, per root §10), or amend what we sell.
3. **Every test maps to exactly one register entry — structurally.** One module
   per guarantee, `tests/g<N>_<slug>.rs`, matching `module` in `register.toml`.
   A test can only live in one file, so the mapping cannot rot. A module with no
   entry, or an entry with no module, fails `cargo xtask register --check`.
4. **Every test is a negative test.** Attempt the violation, then assert an
   outcome. Two prefixes, and the difference matters:
   - `holds_*` — the violation must **fail**. Green means the guarantee holds;
     red means regression.
   - `gap_*` — the violation currently **succeeds**, and the test says so. Green
     means the documented gap is unchanged; **red means the gap may have
     closed** — upstream improved and the register is now under-selling us.
     Escalate that too. It is the good kind of finding, and it is the signal to
     raise `claim` in `register.toml` and convert the test to `holds_*`.
5. **`not yet provided` entries still have tests.** G-6 currently documents a
   gap: the test asserts the *known current* behaviour and flips to must-pass
   when upstream isolation lands. Deleting it because it is "not a real test"
   loses the tripwire.
6. **Both lanes run.** Pinned blocks merge. Latest-nightly alerts and does not.
   Do not make the nightly lane blocking to force attention — that trades a
   signal for a stoppage.
7. **Nothing is marked `provided` in the register without a green test here**
   against the pinned release, and nothing is sold to a customer that the
   register does not mark `provided`. This is now enforced rather than trusted:
   status is **derived from the test run**, not written down.
8. **Never hand-edit a generated register table.** The block between
   `<!-- BEGIN:generated-register -->` markers is output. Edit
   `register.toml` for prose, or the tests for evidence. `--check` will catch
   you, but the point is that there is nothing there worth editing.
9. **`register.toml` carries no status field.** It carries `claim` — the
   strongest status an entry may reach. Evidence can fall short of a claim; it
   can never raise one. Raising a claim is a deliberate edit with an author and
   a diff. See `docs/guarantee-register-generation.md`.

10. **These rules are not editable by an agent either.** Root §13 applies to
    this file, and specifically to the temptation it describes: if an assertion
    here will not pass, the assertion is the message. Do not edit rule 1.
<!-- END:conformance-agent-rules -->
