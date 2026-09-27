import Link from "next/link";
import type { CSSProperties, ReactNode } from "react";

import { cn } from "@/lib/utils";

export type DataTableColumn<T> = {
  key: string;
  label: string;
  render: (row: T) => ReactNode;
  className?: string;
};

export function DataTable<T>({ ariaLabel, columns, gridTemplateColumns, minWidth = "760px", rowHref, rowKey, rows }: Readonly<{
  ariaLabel: string;
  columns: ReadonlyArray<DataTableColumn<T>>;
  gridTemplateColumns: string;
  minWidth?: string;
  rowHref: (row: T) => string;
  rowKey: (row: T) => string;
  rows: ReadonlyArray<T>;
}>) {
  const gridStyle = { gridTemplateColumns } satisfies CSSProperties;
  return (
    <div aria-label={ariaLabel} className="overflow-x-auto rounded-panel border bg-panel" role="table">
      <div style={{ minWidth }}>
        <div className="grid items-center gap-4 border-b bg-canvas px-4 py-2.5 text-xs font-semibold text-muted-ink" role="row" style={gridStyle}>
          {columns.map((column) => <span className={column.className} key={column.key} role="columnheader">{column.label}</span>)}
        </div>
        <div role="rowgroup">
          {rows.map((row) => (
            <Link
              className="grid min-h-16 items-center gap-4 border-b px-4 py-3 text-sm transition-colors last:border-b-0 hover:bg-canvas focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-brand"
              href={rowHref(row)}
              key={rowKey(row)}
              style={gridStyle}
            >
              {columns.map((column) => <span className={cn("min-w-0", column.className)} key={column.key}>{column.render(row)}</span>)}
            </Link>
          ))}
        </div>
      </div>
    </div>
  );
}
