import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { BrowserRunView } from "@/api-client";

import { pollRerun, RunCards } from "./run-views";

function run(overrides: Partial<BrowserRunView>): BrowserRunView {
  return {
    codingAgentRuntime: "codex@0.140.0",
    createdAt: "2026-08-25T20:00:00Z",
    finalizationRequested: false,
    finalized: true,
    phase: "succeeded",
    runtimeOwnership: "provisioned",
    stages: [],
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

describe("GitHub reruns", () => {
  test("polls a pending rerun with one idempotent request until its task is correlated", async () => {
    const attempts = [
      { data: { retryAfterMs: 750 }, response: { ok: true, status: 202 } },
      { data: { taskUid: "task-rerun" }, response: { ok: true, status: 201 } },
    ];
    const waits: number[] = [];
    let calls = 0;

    const outcome = await pollRerun(
      async () => {
        const result = attempts[calls];
        calls += 1;
        if (!result) throw new Error("unexpected poll");
        return result;
      },
      async (milliseconds) => { waits.push(milliseconds); },
    );

    expect(outcome).toEqual({ taskUid: "task-rerun" });
    expect(calls).toBe(2);
    expect(waits).toEqual([750]);
  });

  test("stops polling on a terminal mutation failure", async () => {
    const outcome = await pollRerun(
      async () => ({ response: { ok: false, status: 409 } }),
      async () => { throw new Error("must not wait"); },
    );

    expect(outcome).toEqual({ failure: "conflict" });
  });

  test("bounds server-controlled retry delays and times out as unavailable", async () => {
    const waits: number[] = [];
    const outcome = await pollRerun(
      async () => ({ data: { retryAfterMs: 60_000 }, response: { ok: true, status: 202 } }),
      async (milliseconds) => { waits.push(milliseconds); },
      2,
    );

    expect(outcome).toEqual({ failure: "unavailable" });
    expect(waits).toEqual([5_000]);
  });
});
