"use client";

import { useEffect, useState } from "react";

import { PageHeader } from "@/components/workspace-ui";
import type { ExecutionLogStream } from "@/data/execution-log";
import { authStartPath } from "@/session/auth-redirect";

type ExecutionLogState =
  | { status: "loading" }
  | { status: "ready"; text: string }
  | { status: "unavailable" }
  | { status: "error" };

export function RunLogView({
  admin = false,
  stream,
  taskUid,
}: Readonly<{
  admin?: boolean;
  stream: ExecutionLogStream;
  taskUid: string;
}>) {
  const [state, setState] = useState<ExecutionLogState>({ status: "loading" });
  const runPath = `${admin ? "/admin/runs" : "/runs"}/${encodeURIComponent(taskUid)}`;

  useEffect(() => {
    let active = true;
    const controller = new AbortController();
    const apiPrefix = admin ? "/admin/api/v1/all-runs" : "/app/api/v1/runs";

    async function load() {
      try {
        const response = await fetch(
          `${apiPrefix}/${encodeURIComponent(taskUid)}/logs/${stream}`,
          {
            cache: "no-store",
            credentials: "same-origin",
            headers: { accept: "text/plain" },
            signal: controller.signal,
          },
        );
        if (!active) return;
        if (response.status === 401) {
          window.location.replace(authStartPath(window.location.pathname));
          return;
        }
        if (response.status === 404) {
          setState({ status: "unavailable" });
          return;
        }
        if (!response.ok) {
          setState({ status: "error" });
          return;
        }
        const text = await response.text();
        if (active) setState({ status: "ready", text });
      } catch (error) {
        if (active && !(error instanceof DOMException && error.name === "AbortError")) {
          setState({ status: "error" });
        }
      }
    }

    void load();
    return () => {
      active = false;
      controller.abort();
    };
  }, [admin, stream, taskUid]);

  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader
        actions={(
          <a
            className="inline-flex min-h-11 items-center rounded-md border px-4 py-2 text-sm font-semibold hover:bg-panel"
            href={runPath}
          >
            Back to run
          </a>
        )}
        description={`Captured ${stream} for Task ${taskUid}.`}
        title={`${stream} log`}
      />
      <section aria-label="Execution log" className="space-y-4 rounded-panel border bg-panel p-6 shadow-sm">
        <h2 className="text-xl font-semibold">{stream} log</h2>
        {state.status === "loading" ? (
          <p className="text-sm text-muted-ink" role="status">Loading {stream} log…</p>
        ) : null}
        {state.status === "unavailable" ? (
          <p className="text-sm text-muted-ink" role="status">{stream} log is unavailable for this run.</p>
        ) : null}
        {state.status === "error" ? (
          <p className="text-sm text-red-800" role="alert">The {stream} log could not be loaded.</p>
        ) : null}
        {state.status === "ready" ? (
          <>
            <div className="rounded-md border border-amber-700/60 bg-amber-950/20 p-3 text-sm">
              <p className="font-semibold">Sensitive output warning</p>
              <p className="mt-1 text-muted-ink">Execution logs may reproduce arbitrary user, tool, or agent output.</p>
            </div>
            <pre className="max-h-[70vh] overflow-auto whitespace-pre-wrap break-words rounded-md border bg-canvas p-4 font-mono text-xs">{state.text}</pre>
          </>
        ) : null}
      </section>
    </section>
  );
}
