export type DiagnosticItem = {
  label: string;
  value: unknown;
};

export function DiagnosticDetails({ className = "", items }: Readonly<{ className?: string; items: readonly DiagnosticItem[] }>) {
  const visible = items.flatMap(({ label, value }) => typeof value === "string" && value.trim() ? [{ label, value }] : []);
  if (!visible.length) return null;
  return (
    <details className={`text-xs text-muted-ink ${className}`}>
      <summary className="cursor-pointer font-semibold">Details</summary>
      <dl className="mt-2 grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1">
        {visible.map(({ label, value }) => <div className="contents" key={`${label}-${value}`}><dt>{label}</dt><dd className="break-words font-mono">{value}</dd></div>)}
      </dl>
    </details>
  );
}
