"use client";

import { useState } from "react";

import { GrantChip, type GrantKind } from "./grant-chip";

export type TagSelectOption = { key: string; kind: GrantKind; label: string };

export function TagSelect({ disabled = false, label, onChange, options, value }: Readonly<{
  disabled?: boolean;
  label: string;
  onChange: (next: Array<string>) => void;
  options: ReadonlyArray<TagSelectOption>;
  value: ReadonlyArray<string>;
}>) {
  const [filter, setFilter] = useState("");
  const [open, setOpen] = useState(false);
  const selected = new Set(value);
  const visible = options.filter((option) => option.label.toLowerCase().includes(filter.toLowerCase()));
  const toggle = (key: string) => onChange(selected.has(key) ? value.filter((item) => item !== key) : [...value, key]);
  return (
    <div className="relative" onBlur={(event) => { if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false); }}>
      <div className={`flex min-h-[42px] flex-wrap items-center gap-1.5 rounded-control border bg-panel p-1.5 ${open ? "border-brand ring-3 ring-brand-soft" : "border-field"}`}>
        {value.map((key) => {
          const option = options.find((item) => item.key === key);
          return option ? <GrantChip key={key} kind={option.kind} name={option.label} onRemove={() => toggle(key)} /> : null;
        })}
        <input aria-label={label} className="min-w-36 flex-1 bg-transparent px-1.5 text-sm outline-none" disabled={disabled} onChange={(event) => { setFilter(event.target.value); setOpen(true); }} onFocus={() => setOpen(true)} placeholder={value.length ? "Add more…" : "Search…"} value={filter} />
      </div>
      {open && !disabled ? <div className="absolute z-20 mt-1 max-h-64 w-full overflow-y-auto rounded-control border bg-panel p-1 shadow-lg" role="listbox">{visible.length ? visible.map((option) => <button aria-selected={selected.has(option.key)} className="flex min-h-9 w-full items-center justify-between rounded-control px-3 text-left text-sm hover:bg-subtle" key={option.key} onClick={() => toggle(option.key)} role="option" type="button"><span>{option.label}</span><span aria-hidden="true">{selected.has(option.key) ? "✓" : ""}</span></button>) : <p className="px-3 py-4 text-center text-sm text-muted-ink">No matches</p>}</div> : null}
    </div>
  );
}
