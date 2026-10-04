"use client";

import { useEffect, useState } from "react";

import { PageHeader } from "@/components/workspace-ui";
import type { BrowserExecutionLogResponse } from "@/api-client";
import type { ExecutionLogStream } from "@/data/execution-log";
import { authStartPath } from "@/session/auth-redirect";

type ExecutionLogState =
  | { status: "loading" }
  | { status: "ready"; complete: boolean; text: string }
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
  const runPath = `${admin ? "/admin/runs" : "/runs"}/${encodeURIComponent(taskUid)}?job=agent_execution&stream=${stream}`;

  useEffect(() => {
    let active = true;
    const controller = new AbortController();
    const apiPrefix = admin ? "/admin/api/v1/all-runs" : "/app/api/v1/runs";
    let offset = 0;
    let content = "";
    let timer: ReturnType<typeof setTimeout> | undefined;

    async function load() {
      try {
        const response = await fetch(
          `${apiPrefix}/${encodeURIComponent(taskUid)}/logs/${stream}?after=${offset}`,
          {
            cache: "no-store",
            credentials: "same-origin",
            headers: { accept: "application/json" },
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
        const chunk = await response.json() as BrowserExecutionLogResponse;
        content += chunk.content;
        offset = chunk.truncated
          ? offset + new TextEncoder().encode(chunk.content).byteLength
          : chunk.sizeBytes;
        if (active) setState({ status: "ready", complete: chunk.complete, text: content });
        if (active && (!chunk.complete || chunk.truncated)) timer = setTimeout(() => void load(), chunk.truncated ? 0 : 2000);
      } catch (error) {
        if (active && !(error instanceof DOMException && error.name === "AbortError")) {
          setState({ status: "error" });
        }
      }
    }

    void load();
    return () => {
      active = false;
      if (timer) clearTimeout(timer);
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
        <ExecutionLogContent state={state} stream={stream} warning />
      </section>
    </section>
  );
}

function ExecutionLogContent({ state, stream, warning = false }: Readonly<{
  state: ExecutionLogState;
  stream: ExecutionLogStream;
  warning?: boolean;
}>) {
  if (state.status === "loading") return <p className="text-sm text-muted-ink" role="status">Loading {stream} log…</p>;
  if (state.status === "unavailable") return <div className="space-y-1 text-sm text-muted-ink" role="status"><p className="font-semibold text-ink">No execution log was captured for this run.</p><p>Enable <strong>Capture execution log</strong> when starting a run to retain stdout and stderr.</p></div>;
  if (state.status === "error") return <p className="text-sm text-err" role="alert">The {stream} log could not be loaded.</p>;
  if (state.complete && state.text === "") return <p className="text-sm text-muted-ink" role="status">The captured {stream} log is empty.</p>;
  const lines = state.text.match(/[^\n]*\n|[^\n]+$/g) ?? [state.text];
  return (
    <>
      {warning ? <div className="rounded-control border border-warn/30 bg-warn-soft p-3 text-sm"><p className="font-semibold text-warn">Sensitive output warning</p><p className="mt-1 text-muted-ink">Execution logs may reproduce arbitrary user, tool, or agent output.</p></div> : null}
      <pre aria-label={`${stream} log lines`} className="max-h-[70vh] min-w-0 overflow-auto rounded-control border border-line bg-[#111318] py-3 font-mono text-xs leading-5 text-[#e8eaf0] [counter-reset:line]">{lines.map((line, index) => <span className="grid min-w-max grid-cols-[3.5rem_minmax(0,1fr)] [counter-increment:line] before:select-none before:border-r before:border-white/10 before:px-3 before:text-right before:text-white/35 before:content-[counter(line)]" key={`${index}:${line}`}><span className="whitespace-pre-wrap break-words px-3">{line || " "}</span></span>)}</pre>
      {!state.complete ? <p className="text-xs text-muted-ink" role="status">Live log · polling for new output…</p> : null}
    </>
  );
}

export function ExecutionLogPanel({ admin = false, stream, taskUid }: Readonly<{
  admin?: boolean;
  stream: ExecutionLogStream;
  taskUid: string;
}>) {
  const [state, setState] = useState<ExecutionLogState>({ status: "loading" });

  useEffect(() => {
    let active = true;
    const controller = new AbortController();
    const apiPrefix = admin ? "/admin/api/v1/all-runs" : "/app/api/v1/runs";
    let offset = 0;
    let content = "";
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function load() {
      try {
        const response = await fetch(`${apiPrefix}/${encodeURIComponent(taskUid)}/logs/${stream}?after=${offset}`, { cache: "no-store", credentials: "same-origin", headers: { accept: "application/json" }, signal: controller.signal });
        if (!active) return;
        if (response.status === 401) { window.location.replace(authStartPath(window.location.pathname)); return; }
        if (response.status === 404) { setState({ status: "unavailable" }); return; }
        if (!response.ok) { setState({ status: "error" }); return; }
        const chunk = await response.json() as BrowserExecutionLogResponse;
        content += chunk.content;
        offset = chunk.truncated ? offset + new TextEncoder().encode(chunk.content).byteLength : chunk.sizeBytes;
        setState({ status: "ready", complete: chunk.complete, text: content });
        if (!chunk.complete || chunk.truncated) timer = setTimeout(() => void load(), chunk.truncated ? 0 : 2000);
      } catch (error) {
        if (active && !(error instanceof DOMException && error.name === "AbortError")) setState({ status: "error" });
      }
    }
    void load();
    return () => { active = false; if (timer) clearTimeout(timer); controller.abort(); };
  }, [admin, stream, taskUid]);

  return <ExecutionLogContent state={state} stream={stream} />;
}
