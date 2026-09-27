import type { ReactNode } from "react";

export function StatStrip({ items }: Readonly<{
  items: ReadonlyArray<{ detail?: ReactNode; label: string; value: ReactNode }>;
}>) {
  return (
    <dl className="grid overflow-hidden rounded-card border bg-panel sm:grid-cols-2 lg:grid-cols-4">
      {items.map((item) => (
        <div className="min-w-0 border-b border-line-soft px-5 py-4 last:border-b-0 sm:border-e sm:[&:nth-child(2n)]:border-e-0 lg:border-b-0 lg:[&:nth-child(2n)]:border-e lg:last:border-e-0" key={item.label}>
          <dt className="text-xs font-semibold text-muted-ink">{item.label}</dt>
          <dd className="mt-1 truncate text-lg font-semibold text-ink">{item.value}</dd>
          {item.detail ? <div className="mt-2 text-xs text-muted-ink">{item.detail}</div> : null}
        </div>
      ))}
    </dl>
  );
}
