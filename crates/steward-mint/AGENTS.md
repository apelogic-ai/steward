<!-- BEGIN:mint-agent-rules -->
# This crate holds the signing key

Every HOP-1 token in the system is trusted because it was signed here. A defect
in this crate is not a bug in one feature; it is a defect in every access
decision the platform has ever made.

**Human review is required for any change under this path.** Do not open a PR
here as part of a larger change — split it out.

1. **Never log, return, or serialise a private key, an SVID, or a signed token
   payload.** Not at debug level. Not in an error message. The type wrapping key
   material must not implement `Debug` or `Display` in a way that reveals it.
   The private signing key never touches the working tree — not as a fixture,
   not as a test artifact, not temporarily (root §11.1). Tests generate an
   ephemeral key at runtime and drop it with the guard that created it.
2. **Never widen the claim set without updating the claim contract doc** and
   every consumer that keys off it — `mcp-gw` HOP-1 config, the git gateway, the
   notification block. Re-cutting this contract is expensive and has already
   been paid for twice.
3. **The mint takes a `Principal`, never a bare email.** The `Service` arm is
   `unimplemented!()` in v0.1.0 and stays that way until the service-principal
   work is scheduled. Do not implement it opportunistically because a test
   needed it.
4. **TTLs are short and never widened to make a test pass.** If a test needs a
   longer life, the test is wrong.
5. **Negative tests first, without exception here.** Forged SVID, expired SVID,
   SVID for a different workload, replay after revocation, token for a
   terminated runtime. Each must have a test that requires failure before the
   corresponding positive path is written.
6. **Revocation is not eventual.** A change that makes revocation take effect
   "on next refresh" breaks G-4 and the S5 exit criterion.

7. **Root §13 applies here with force.** If a rule in this file blocks a change,
   that is the file working. Escalate; do not edit it.
<!-- END:mint-agent-rules -->
