import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { BrowserRunView } from "@/api-client";

import { RunCards } from "./run-views";

function run(overrides: Partial<BrowserRunView>): BrowserRunView {
  return {
    codingAgentRuntime: "codex@0.117.0",
    createdAt: "2026-08-25T20:00:00Z",
    finalizationRequested: false,
    finalized: true,
    phase: "succeeded",
    runtimeOwnership: "provisioned",
    taskUid: "task-default",
    updatedAt: "2026-08-25T20:00:00Z",
    workflow: "test-wf@3",
    ...overrides,
  };
}

describe("run cards", () => {
  test("renders one column newest-first with conventional outcome colors", () => {
    const html = renderToStaticMarkup(<RunCards runs={[
      run({ taskUid: "task-older", runtimeUid: "oldruntime-0000-0000-0000-000000000000", updatedAt: "2026-08-25T20:00:00Z", phase: "failed" }),
      run({ taskUid: "task-newer", runtimeUid: "newruntime-0000-0000-0000-000000000000", updatedAt: "2026-08-25T21:00:00Z", phase: "succeeded" }),
    ]} />);

    expect(html.indexOf("newruntime")).toBeLessThan(html.indexOf("oldruntime"));
    expect(html).toContain('<ul class="grid gap-4">');
    expect(html).toContain("rounded-panel border bg-panel px-5 py-4 shadow-sm");
    expect(html).toContain("status-badge-success");
    expect(html).toContain("status-badge-error");
    expect(html).toContain('<div class="mt-4"><dl');
    expect(html).not.toContain("mt-5 inline-flex");
    expect(html).not.toContain(">task-newer</p>");
    expect(html).not.toContain(">task-older</p>");
    expect(html).not.toContain("newruntime-0000");
    expect(html).not.toContain("oldruntime-0000");
    expect(html).not.toContain("uppercase");
  });
});
