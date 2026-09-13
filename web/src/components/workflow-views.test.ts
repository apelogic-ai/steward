import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

const source = readFileSync(new URL("./workflow-views.tsx", import.meta.url), "utf8");

describe("Workflow agent catalog", () => {
  test("shows a bounded empty state instead of an unusable authoring form", () => {
    expect(source).toContain('agents.length === 0');
    expect(source).toContain('title="No coding agents configured"');
  });

  test("uses deployment presentation metadata without changing submitted identity", () => {
    expect(source).toContain("agent.displayName ?? agent.agentRef");
    expect(source).toContain('value={agent.agentRef}');
  });

  test("authors an explicit model for an immutable Workflow revision", () => {
    expect(source).toContain('name="modelProvider"');
    expect(source).toContain('name="modelName"');
    expect(source).toContain("model: {");
  });
});
