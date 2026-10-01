import { describe, expect, test } from "bun:test";

import type { CapabilityTool } from "@/api-client";

import { clearTools, dedupeToolGrants, groupCapabilityTools, selectReadOnlyTools, toolSelectionDiff } from "./tool-picker";

const tools: Array<CapabilityTool> = [
  { provider: "github", resource: "issues_get", action: "read", accessClass: "read", toolsets: ["issues"] },
  { provider: "github", resource: "repository_get", action: "read", accessClass: "read", toolsets: ["repositories", "search"] },
  { provider: "github", resource: "issues_update", action: "write", accessClass: "write", toolsets: ["issues"] },
  { provider: "github", resource: "repository_delete", action: "delete", accessClass: "destructive" },
];

describe("tool picker authority helpers", () => {
  test("uses only authoritative multi-membership toolsets and one deterministic fallback", () => {
    expect(groupCapabilityTools(tools).map((group) => ({
      authoritative: group.authoritative,
      kind: group.kind,
      name: group.name,
      resources: group.tools.map((tool) => tool.resource),
    }))).toEqual([
      { authoritative: true, kind: "toolset", name: "issues", resources: ["issues_get", "issues_update"] },
      { authoritative: true, kind: "toolset", name: "repositories", resources: ["repository_get"] },
      { authoritative: true, kind: "toolset", name: "search", resources: ["repository_get"] },
      { authoritative: false, kind: "fallback", name: "Ungrouped", resources: ["repository_delete"] },
    ]);

    expect(groupCapabilityTools(tools.map((tool) => ({
      provider: tool.provider,
      resource: tool.resource,
      action: tool.action,
      accessClass: tool.accessClass,
    }))))
      .toMatchObject([{ authoritative: false, name: "All tools" }]);
  });

  test("keeps an authoritative Ungrouped toolset distinct from the ungrouped fallback", () => {
    const mixed: Array<CapabilityTool> = [
      { provider: "github", resource: "authoritative", action: "read", accessClass: "read", toolsets: ["Ungrouped"] },
      { provider: "github", resource: "legacy", action: "read", accessClass: "read" },
    ];

    expect(groupCapabilityTools(mixed).map((group) => ({
      authoritative: group.authoritative,
      kind: group.kind,
      name: group.name,
      resources: group.tools.map((tool) => tool.resource),
    }))).toEqual([
      { authoritative: true, kind: "toolset", name: "Ungrouped", resources: ["authoritative"] },
      { authoritative: false, kind: "fallback", name: "No authoritative toolset", resources: ["legacy"] },
    ]);
  });

  test("bulk selection adds only read tools and deduplicates exact authority tuples", () => {
    const selected = selectReadOnlyTools([
      { provider: "github", resource: "repository_get", action: "read" },
    ], tools);
    expect(selected).toEqual([
      { provider: "github", resource: "issues_get", action: "read" },
      { provider: "github", resource: "repository_get", action: "read" },
    ]);
    expect(dedupeToolGrants([...selected, selected[0]])).toEqual(selected);
  });

  test("group clearing and revision diffs operate on exact tuples", () => {
    const selected = tools.map(({ provider, resource, action }) => ({ provider, resource, action }));
    expect(clearTools(selected, tools.filter((tool) => tool.toolsets?.includes("issues"))))
      .toEqual(selected.filter((tool) => !tool.resource.startsWith("issues_")));
    expect(toolSelectionDiff(selected.slice(0, 2), selected.slice(1, 3))).toEqual({
      added: [{ provider: "github", resource: "issues_update", action: "write" }],
      removed: [{ provider: "github", resource: "issues_get", action: "read" }],
    });
  });
});
