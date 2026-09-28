export function FilterChips({ active, items, onChange }: Readonly<{
  active: string;
  items: ReadonlyArray<{ count?: number; label: string; value: string }>;
  onChange: (value: string) => void;
}>) {
  return (
    <div aria-label="Filters" className="flex flex-wrap gap-2" role="group">
      {items.map((item) => {
        const selected = active === item.value;
        return <button aria-pressed={selected} className={`inline-flex min-h-9 items-center gap-2 rounded-full border px-3.5 py-1.5 text-sm font-semibold ${selected ? "border-brand bg-brand-soft text-ink" : "border-line bg-panel text-muted-ink hover:bg-subtle hover:text-ink"}`} key={item.value} onClick={() => onChange(item.value)} type="button"><span>{item.label}</span>{item.count === undefined ? null : <span className="font-mono text-xs text-faint-ink">{item.count}</span>}</button>;
      })}
    </div>
  );
}
