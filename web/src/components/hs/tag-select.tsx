"use client";

import { useId, useMemo, useState, type KeyboardEvent } from "react";

import { GrantChip, type GrantKind } from "./grant-chip";

export type TagSelectOption = {
  disabled?: boolean;
  key: string;
  kind: GrantKind;
  label: string;
  note?: string;
};

const MAX_VISIBLE_OPTIONS = 40;

export function TagSelect({
  addPlaceholder = "Add another…",
  allowCreate = false,
  createKind = "neutral",
  disabled = false,
  emptyPlaceholder = "Search…",
  inputDisabled = false,
  label,
  onChange,
  options,
  value,
}: Readonly<{
  addPlaceholder?: string;
  allowCreate?: boolean;
  createKind?: GrantKind;
  disabled?: boolean;
  emptyPlaceholder?: string;
  inputDisabled?: boolean;
  label: string;
  onChange: (next: Array<string>) => void;
  options: ReadonlyArray<TagSelectOption>;
  value: ReadonlyArray<string>;
}>) {
  const listboxId = useId();
  const [activeIndex, setActiveIndex] = useState(0);
  const [filter, setFilter] = useState("");
  const [open, setOpen] = useState(false);
  const selected = useMemo(() => new Set(value), [value]);
  const available = useMemo(() => {
    const query = filter.trim().toLocaleLowerCase();
    return options.filter((option) => !selected.has(option.key)
      && (!query || option.label.toLocaleLowerCase().includes(query) || option.note?.toLocaleLowerCase().includes(query)));
  }, [filter, options, selected]);
  const createValue = filter.trim();
  const createOption: TagSelectOption | null = allowCreate && createValue && !selected.has(createValue) && !options.some((option) => option.key === createValue)
    ? { key: createValue, kind: createKind, label: createValue, note: "Press Enter to add" }
    : null;
  const visible = [...(createOption ? [createOption] : []), ...available].slice(0, MAX_VISIBLE_OPTIONS);
  const selectedOptions = value.flatMap((key) => {
    const option = options.find((item) => item.key === key);
    return option ? [option] : [];
  });

  const choose = (option: TagSelectOption) => {
    if (option.disabled) return;
    onChange([...value, option.key]);
    setFilter("");
    setActiveIndex(0);
  };
  const remove = (key: string) => onChange(value.filter((item) => item !== key));
  const moveActive = (direction: 1 | -1) => {
    if (visible.length === 0) return;
    let next = activeIndex;
    for (let attempts = 0; attempts < visible.length; attempts += 1) {
      next = (next + direction + visible.length) % visible.length;
      if (!visible[next]?.disabled) break;
    }
    setActiveIndex(next);
  };
  const handleKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Escape") {
      setOpen(false);
      return;
    }
    if (event.key === "Backspace" && filter === "" && value.length > 0) {
      event.preventDefault();
      remove(value[value.length - 1]);
      return;
    }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      setOpen(true);
      moveActive(event.key === "ArrowDown" ? 1 : -1);
      return;
    }
    if (event.key === "Enter" && open && visible[activeIndex]) {
      event.preventDefault();
      choose(visible[activeIndex]);
    }
  };
  return (
    <div className="relative" onBlur={(event) => { if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false); }}>
      <div className={`flex min-h-[42px] flex-wrap items-center gap-1.5 rounded-control border bg-panel px-1.5 py-[5px] ${open ? "border-brand ring-3 ring-brand-soft" : "border-field"}`}>
        {selectedOptions.map((option) => <GrantChip key={option.key} kind={option.kind} name={option.label} onRemove={disabled ? undefined : () => remove(option.key)} />)}
        <input
          aria-activedescendant={open && visible[activeIndex] ? `${listboxId}-${activeIndex}` : undefined}
          aria-autocomplete="list"
          aria-controls={listboxId}
          aria-expanded={open}
          aria-label={label}
          className="min-w-36 flex-1 bg-transparent px-1.5 py-1 text-sm outline-none placeholder:text-faint-ink"
          disabled={disabled || inputDisabled}
          onChange={(event) => { setFilter(event.target.value); setActiveIndex(0); setOpen(true); }}
          onFocus={() => setOpen(true)}
          onKeyDown={handleKeyDown}
          placeholder={value.length ? addPlaceholder : emptyPlaceholder}
          role="combobox"
          value={filter}
        />
      </div>
      {open && !disabled && !inputDisabled ? <div className="absolute z-20 mt-1 max-h-72 w-full overflow-y-auto rounded-control border bg-panel p-1 shadow-lg" id={listboxId} role="listbox">{visible.length ? visible.map((option, index) => <button aria-disabled={option.disabled || undefined} aria-selected={index === activeIndex} className={`flex min-h-10 w-full items-center justify-between gap-4 rounded-control px-3 text-left text-sm ${option.disabled ? "cursor-not-allowed text-faint-ink" : index === activeIndex ? "bg-brand-soft" : "hover:bg-subtle"}`} disabled={option.disabled} id={`${listboxId}-${index}`} key={option.key} onMouseDown={(event) => event.preventDefault()} onMouseEnter={() => setActiveIndex(index)} onClick={() => choose(option)} role="option" type="button"><span className="font-mono">{option.label}</span><span className="flex items-center gap-2">{option.note ? <span className="text-xs text-muted-ink">{option.note}</span> : null}<span className="rounded-full bg-line-soft px-2 py-0.5 text-[11px] font-semibold capitalize text-muted-ink">{option.kind}</span></span></button>) : <p className="px-3 py-4 text-center text-sm text-muted-ink">{filter ? `No match for “${filter}”` : "Everything available is selected"}</p>}{available.length + (createOption ? 1 : 0) > MAX_VISIBLE_OPTIONS ? <p className="border-t border-line-soft px-3 py-2 text-xs text-muted-ink">{available.length + (createOption ? 1 : 0) - MAX_VISIBLE_OPTIONS} more — keep typing to narrow</p> : null}</div> : null}
    </div>
  );
}
