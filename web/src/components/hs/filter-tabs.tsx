import { cn } from "@/lib/utils";

export function FilterTabs({ active, items, onChange }: Readonly<{
  active: string;
  items: ReadonlyArray<{ count?: number; label: string; value: string }>;
  onChange: (value: string) => void;
}>) {
  return (
    <div aria-label="Filter results" className="flex max-w-full gap-1 overflow-x-auto border-b" role="tablist">
      {items.map((item) => {
        const selected = item.value === active;
        return (
          <button
            aria-selected={selected}
            className={cn("flex h-10 shrink-0 items-center gap-2 border-b-2 px-3 text-sm font-medium", selected ? "border-brand text-ink" : "border-transparent text-muted-ink hover:text-ink")}
            key={item.value}
            onClick={() => onChange(item.value)}
            role="tab"
            type="button"
          >
            {item.label}
            {item.count === undefined ? null : <span className="rounded-full bg-line-soft px-2 py-0.5 text-xs tabular-nums">{item.count}</span>}
          </button>
        );
      })}
    </div>
  );
}
