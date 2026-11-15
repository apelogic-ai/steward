# mcp-gw sandbox helper source

Source: `apelogic-ai/mcp-gw` release `v0.8.2`, commit
`cff55e2b271273515722df0f619b9a8532b33b81`, files
`servers/governed-cli/src/sandbox-helper.ts` and
`servers/governed-cli/src/shim.ts`, unmodified. License: MIT (`LICENSE`).

## Why it is vendored

The Codex reference runtime image (`build/codex-reference.Dockerfile`) compiles
`mcp-gw-sandbox-helper` from these files with `bun build --compile` in a pinned
Bun 1.2.21 build stage, instead of downloading the prebuilt binary that mcp-gw
publishes. Anyone who builds the image from this repository, including
installations that must build every image in their own CI, gets a helper built
from source, and no prebuilt artifact of ours enters the image. The compiled
helper embeds the Bun 1.2.21 runtime from the pinned `oven/bun` image.

The helper uses only Node built-ins (`node:fs`, `node:path`), so the build needs
no package install and no network beyond the pinned Bun image.

## Integrity

`xtask` (`vendored_sandbox_helper_source_matches_its_pinned_release`) checks
that each file, and `LICENSE`, still has its pinned SHA-256:

| File | SHA-256 |
|---|---|
| `sandbox-helper.ts` | `22eca9be6fe9cb87f36a923364f19adda94a84c04c9d350c2e0f1808b4cd0589` |
| `shim.ts` | `88c08c2822213311567cd2e3f319fb4fbb2ba0ce4d2eca66986689c8e924228e` |
| `LICENSE` | `cb1214bf664cc90a56b7584f8a758d83014ed55ff7f65cacf2078d8dea1d39b3` |

## Updating

Move to a new mcp-gw release by copying the same two files and `LICENSE` from
its release commit into a new directory named after the commit's first eight
characters, and remove this directory. In the same change, update:

- the table above;
- the paths in `build/codex-reference.Dockerfile`;
- the `xtask` contract test (`governed_command_helper_is_pinned_packaged_and_tested`)
  and the SHA-256 pin test, including its directory and commit;
- the MCP-GW version in `docs/installation/codex-reference-runtime.md`, which the
  contract test checks;
- `THIRD_PARTY_NOTICES.md` and `CHANGELOG.md`.

Check that the new files still import only Node built-ins. If they need a
package, the build stage needs a lockfile and an install, and the network
statement above no longer holds. Do not edit these files in place: a change to
the helper belongs in mcp-gw first.

Remove this directory when the reference image no longer ships the helper.
