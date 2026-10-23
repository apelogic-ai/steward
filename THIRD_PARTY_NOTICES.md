# Third-party notices

Steward's own source and chart are licensed under the root [MIT license](LICENSE).
The following checked-in patch material retains its upstream license and
copyright; the Steward MIT notice does not replace those terms.

| Material | Pinned upstream source | License copy |
|---|---|---|
| MCP-GW verified-claims patch | [apelogic-ai/mcp-gw `c2af10d9`](https://github.com/apelogic-ai/mcp-gw/tree/c2af10d9c3dee898e368e6cf3d0f5a1ef6ad0dde) | [MIT, Copyright (c) 2026 ApeLogic AI](third_party/mcp-gw-patches/c2af10d9/LICENSE) |

The patch README records the exact base, purpose, and removal condition.

The released Claude Code reference runtime redistributes
`@anthropic-ai/claude-code@2.1.222`. Its package metadata identifies the license as
"SEE LICENSE IN README.md"; the package's `LICENSE.md` is retained in the image at
`/usr/lib/node_modules/@anthropic-ai/claude-code/LICENSE.md` and states that use is subject to
Anthropic's linked legal agreements. The Steward MIT license does not replace those terms.

Redistributed Rust crates, JavaScript packages, base images, and operating
system packages retain their own licenses. The release workflow produces an
SPDX software bill of materials for each image and the chart; review that
artifact's exact dependency set and carry any required upstream license and
notice texts when redistributing it. This source notice is not a substitute
for the artifact-specific dependency license bundle.
