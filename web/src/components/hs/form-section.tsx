import { useId, type ReactNode } from "react";

export function FormSection({ children, description, number, title }: Readonly<{
  children: ReactNode;
  description?: string;
  number?: string;
  title: string;
}>) {
  const titleId = useId();
  return (
    <section aria-labelledby={titleId} className="grid gap-4 border-b border-line-soft py-5 last:border-b-0 sm:grid-cols-[180px_minmax(0,1fr)]" role="group">
      <div><h2 className="text-sm font-semibold" id={titleId}>{number ? <span className="me-2 font-mono text-faint-ink">{number}</span> : null}{title}</h2>{description ? <p className="mt-1 text-xs leading-5 text-muted-ink">{description}</p> : null}</div>
      <div className="min-w-0">{children}</div>
    </section>
  );
}
