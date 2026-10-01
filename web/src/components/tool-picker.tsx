"use client";

import { useMemo, useState } from "react";

import type { CapabilityTool, ToolGrant } from "@/api-client";

type ToolAccessClass = CapabilityTool["accessClass"];

export type ToolGroup = {
  authoritative: boolean;
  name: string;
  tools: Array<CapabilityTool>;
};

export function toolKey(tool: Pick<ToolGrant, "provider" | "resource" | "action">): string {
  return JSON.stringify([tool.provider, tool.resource, tool.action]);
}

function compareTools(left: Pick<ToolGrant, "provider" | "resource" | "action">, right: Pick<ToolGrant, "provider" | "resource" | "action">): number {
  return toolKey(left).localeCompare(toolKey(right));
}

export function dedupeToolGrants(tools: ReadonlyArray<ToolGrant>): Array<ToolGrant> {
  const byKey = new Map<string, ToolGrant>();
  for (const tool of tools) {
    const key = toolKey(tool);
    if (!byKey.has(key)) byKey.set(key, { provider: tool.provider, resource: tool.resource, action: tool.action });
  }
  return [...byKey.values()].sort(compareTools);
}

export function groupCapabilityTools(tools: ReadonlyArray<CapabilityTool>): Array<ToolGroup> {
  const sorted = [...tools].sort(compareTools);
  const groups = new Map<string, Array<CapabilityTool>>();
  const ungrouped: Array<CapabilityTool> = [];
  for (const tool of sorted) {
    const names = [...new Set(tool.toolsets ?? [])].sort((left, right) => left.localeCompare(right));
    if (names.length === 0) {
      ungrouped.push(tool);
      continue;
    }
    for (const name of names) groups.set(name, [...(groups.get(name) ?? []), tool]);
  }
  if (groups.size === 0) return [{ authoritative: false, name: "All tools", tools: sorted }];
  const result = [...groups.entries()]
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([name, groupTools]) => ({ authoritative: true, name, tools: groupTools }));
  if (ungrouped.length > 0) result.push({ authoritative: false, name: "Ungrouped", tools: ungrouped });
  return result;
}

export function selectReadOnlyTools(current: ReadonlyArray<ToolGrant>, available: ReadonlyArray<CapabilityTool>): Array<ToolGrant> {
  return dedupeToolGrants([
    ...current,
    ...available.filter((tool) => tool.accessClass === "read"),
  ]);
}

export function clearTools(current: ReadonlyArray<ToolGrant>, removed: ReadonlyArray<CapabilityTool>): Array<ToolGrant> {
  const removedKeys = new Set(removed.map(toolKey));
  return current.filter((tool) => !removedKeys.has(toolKey(tool)));
}

export function toolSelectionDiff(previous: ReadonlyArray<ToolGrant>, next: ReadonlyArray<ToolGrant>): { added: Array<ToolGrant>; removed: Array<ToolGrant> } {
  const previousKeys = new Set(previous.map(toolKey));
  const nextKeys = new Set(next.map(toolKey));
  return {
    added: dedupeToolGrants(next.filter((tool) => !previousKeys.has(toolKey(tool)))),
    removed: dedupeToolGrants(previous.filter((tool) => !nextKeys.has(toolKey(tool)))),
  };
}

function toolLabel(tool: Pick<ToolGrant, "provider" | "resource" | "action">): string {
  return `${tool.provider}:${tool.resource}:${tool.action}`;
}

function accessBadgeClass(accessClass: ToolAccessClass): string {
  return accessClass === "read"
    ? "bg-ok-soft text-ok"
    : accessClass === "write"
      ? "bg-warn-soft text-warn"
      : "bg-err-soft text-err";
}

export function ToolPicker({ catalog, missingTools, onChange, previousTools, tools }: Readonly<{
  catalog: ReadonlyArray<CapabilityTool>;
  missingTools: ReadonlyArray<ToolGrant>;
  onChange: (tools: Array<ToolGrant>) => void;
  previousTools?: ReadonlyArray<ToolGrant>;
  tools: ReadonlyArray<ToolGrant>;
}>) {
  const [query, setQuery] = useState("");
  const selected = useMemo(() => new Set(tools.map(toolKey)), [tools]);
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const groups = useMemo(() => groupCapabilityTools(catalog), [catalog]);
  const visibleGroups = groups.map((group) => ({
    ...group,
    tools: group.tools.filter((tool) => !normalizedQuery || [tool.provider, tool.resource, tool.action, tool.accessClass, ...(tool.toolsets ?? [])]
      .some((value) => value.toLocaleLowerCase().includes(normalizedQuery))),
  })).filter((group) => group.tools.length > 0);
  const counts = catalog.reduce<Record<ToolAccessClass, number>>((result, tool) => {
    if (selected.has(toolKey(tool))) result[tool.accessClass] += 1;
    return result;
  }, { read: 0, write: 0, destructive: 0 });
  const diff = previousTools ? toolSelectionDiff(previousTools, tools) : null;

  function toggle(tool: CapabilityTool) {
    const key = toolKey(tool);
    if (selected.has(key)) {
      onChange(tools.filter((candidate) => toolKey(candidate) !== key));
      return;
    }
    if (tool.accessClass !== "read" && !window.confirm(`Grant ${tool.accessClass} access to ${toolLabel(tool)}?`)) return;
    onChange(dedupeToolGrants([...tools, tool]));
  }

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap gap-2">
        <button className="rounded-control border bg-panel px-3 py-2 text-sm font-semibold" disabled={catalog.length === 0} onClick={() => onChange(selectReadOnlyTools(tools, catalog))} type="button">Select all read-only</button>
        <button className="rounded-control border bg-panel px-3 py-2 text-sm font-semibold" disabled={tools.length === 0} onClick={() => onChange([])} type="button">Clear all</button>
      </div>
      <label className="grid gap-2 text-sm font-semibold">Search tools
        <input className="min-h-11 rounded-control border bg-panel px-3 font-normal" disabled={catalog.length === 0} onChange={(event) => setQuery(event.target.value)} placeholder="Provider, tool, access class, or toolset" type="search" value={query} />
      </label>
      <div className="space-y-4">
        {visibleGroups.map((group) => (
          <section className="rounded-control border" key={group.name}>
            <header className="flex flex-wrap items-center justify-between gap-2 border-b bg-subtle px-3 py-2">
              <h3 className="font-semibold">{group.name}</h3>
              {group.authoritative ? <div className="flex gap-2">
                <button className="text-xs font-semibold text-brand" onClick={() => onChange(selectReadOnlyTools(tools, group.tools))} type="button">Select read-only in {group.name}</button>
                <button className="text-xs font-semibold text-muted-ink" onClick={() => onChange(clearTools(tools, group.tools))} type="button">Clear {group.name}</button>
              </div> : null}
            </header>
            <div className="divide-y divide-line-soft">
              {group.tools.map((tool) => {
                const key = toolKey(tool);
                return <label className="flex min-h-11 cursor-pointer items-center gap-3 px-3 py-2" key={`${group.name}:${key}`}>
                  <input checked={selected.has(key)} onChange={() => toggle(tool)} type="checkbox" />
                  <span className="min-w-0 flex-1 break-all font-mono text-sm">{toolLabel(tool)}</span>
                  <span className={`rounded-full px-2 py-0.5 text-xs font-semibold ${accessBadgeClass(tool.accessClass)}`}>{tool.accessClass}</span>
                </label>;
              })}
            </div>
          </section>
        ))}
        {catalog.length > 0 && visibleGroups.length === 0 ? <p className="rounded-control border px-3 py-4 text-center text-sm text-muted-ink">No tools match “{query}”.</p> : null}
      </div>
      {missingTools.length > 0 ? <div className="rounded-control border border-warn px-3 py-3 text-sm text-warn">
        <p>{missingTools.length} selected tool{missingTools.length === 1 ? " is" : "s are"} not listed in the deployment capability catalog. Remove or replace before saving.</p>
        <ul className="mt-2 space-y-1 font-mono text-xs">{missingTools.map((tool) => <li className="flex items-center justify-between gap-3" key={toolKey(tool)}><span>{toolLabel(tool)}</span><button className="font-sans font-semibold" onClick={() => onChange(tools.filter((candidate) => toolKey(candidate) !== toolKey(tool)))} type="button">Remove unavailable {toolLabel(tool)}</button></li>)}</ul>
      </div> : catalog.length === 0 ? <p className="text-sm text-muted-ink">No tools are listed in the deployment capability catalog.</p> : null}
      <p className="text-sm font-semibold" aria-label="Selected tool access counts">{counts.read} read · {counts.write} write · {counts.destructive} destructive</p>
      {diff ? <section aria-label="Tool changes in next revision" className="rounded-control border bg-subtle px-3 py-3 text-sm">
        <h3 className="font-semibold">Tool changes in next revision</h3>
        {diff.added.length === 0 && diff.removed.length === 0 ? <p className="mt-1 text-muted-ink">No tool authority changes.</p> : <div className="mt-2 grid gap-2 sm:grid-cols-2">
          <div><p className="font-semibold text-ok">Added ({diff.added.length})</p><ul className="mt-1 list-inside list-disc font-mono text-xs">{diff.added.map((tool) => <li key={toolKey(tool)}>{toolLabel(tool)}</li>)}</ul></div>
          <div><p className="font-semibold text-err">Removed ({diff.removed.length})</p><ul className="mt-1 list-inside list-disc font-mono text-xs">{diff.removed.map((tool) => <li key={toolKey(tool)}>{toolLabel(tool)}</li>)}</ul></div>
        </div>}
      </section> : null}
    </div>
  );
}
