import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { BrowserRunView } from "@/api-client";

import { exactRepositoryBundle, pollRerun, rerunFailureMessage, RunCards } from "./run-views";

function run(overrides: Partial<BrowserRunView>): BrowserRunView {
  return {
    codingAgentRuntime: "codex@0.140.0",
    createdAt: "2026-08-25T20:00:00Z",
    finalizationRequested: false,
    finalized: true,
    origin: "unknown",
    phase: "succeeded",
    runtimeOwnership: "provisioned",
    stages: [],
    taskUid: "task-default",
    updatedAt: "2026-08-25T20:00:00Z",
    workflow: "test-wf@3",
    ...overrides,
  };
}

describe("run table", () => {
  test("renders newest-first in an internally scrollable table with conventional outcome colors", () => {
    const html = renderToStaticMarkup(<RunCards runs={[
      run({ taskUid: "task-older", runtimeUid: "oldruntime-0000-0000-0000-000000000000", updatedAt: "2026-08-25T20:00:00Z", phase: "failed" }),
      run({
        taskUid: "task-newer",
        runtimeUid: "newruntime-0000-0000-0000-000000000000",
        updatedAt: "2026-08-25T21:00:00Z",
        phase: "succeeded",
        origin: "browser",
        package: { source: "inline", revision: `steward:sha256:${"a".repeat(64)}`, path: "task-definition.json" },
        userEnvelopeRevision: 7,
      }),
    ]} />);

    expect(html.indexOf("newruntime")).toBeLessThan(html.indexOf("oldruntime"));
    expect(html).toContain('aria-label="Runs"');
    expect(html).toContain('class="overflow-x-auto rounded-panel border bg-panel"');
    expect(html).toContain('role="table"');
    expect(html).toContain('href="/runs/task-newer"');
    expect(html).toContain('data-tone="ok"');
    expect(html).toContain('data-tone="err"');
    expect(html).toContain("task-newer");
    expect(html).toContain("task-older");
    expect(html).toContain("newruntime-0000-0000-0000-000000000000");
    expect(html).toContain("oldruntime-0000-0000-0000-000000000000");
    expect(html).toContain("Origin / package");
    expect(html).toContain("browser");
    expect(html).toContain("steward:sha256:");
    expect(html).toContain("rev 7");
    expect(html).not.toContain("uppercase");
  });
});

test("repository bundle preserves the exact successful package over wrapper files", () => {
  expect(exactRepositoryBundle(
    { "task-definition.json": "exact task", "prompt.md": "exact prompt" },
    { "task-definition.json": "stale task", ".steward/invocations/browser-task.json": "invocation" },
  )).toEqual({
    "task-definition.json": "exact task",
    "prompt.md": "exact prompt",
    ".steward/invocations/browser-task.json": "invocation",
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

  test("preserves the staged-orchestration diagnostic", async () => {
    const outcome = await pollRerun(
      async () => ({
        error: { error: "connections.orchestration_not_active" },
        response: { ok: false, status: 503 },
      }),
      async () => { throw new Error("must not wait"); },
    );

    expect(outcome).toEqual({ failure: "orchestration-not-active" });
    expect(rerunFailureMessage("orchestration-not-active"))
      .toBe("Re-run is disabled until task orchestration is active (stage 2).");
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
