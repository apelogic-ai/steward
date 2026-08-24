import Link from "next/link";
import type { ReactNode } from "react";

import type { ResourceState } from "@/data/use-api-resource";

export function PageHeader({ actions, description, eyebrow, title }: Readonly<{
  actions?: ReactNode;
  description: string;
  eyebrow: string;
  title: string;
}>) {
  return (
    <header className="flex flex-wrap items-end justify-between gap-4">
      <div className="max-w-3xl space-y-2">
        <p className="text-xs font-semibold uppercase tracking-[0.18em] text-brand">{eyebrow}</p>
        <h1 className="text-3xl font-semibold tracking-tight sm:text-4xl" id="page-title">{title}</h1>
        <p className="text-base leading-7 text-muted-ink">{description}</p>
      </div>
      {actions}
    </header>
  );
}

export function PrimaryLink({ children, href }: Readonly<{ children: ReactNode; href: string }>) {
  return <Link className="inline-flex min-h-11 items-center rounded-md bg-brand px-4 py-2 text-sm font-semibold text-white hover:bg-brand-strong" href={href}>{children}</Link>;
}

export function StatusBadge({ value }: Readonly<{ value: string }>) {
  return <span className="inline-flex rounded-full border bg-canvas px-2.5 py-1 text-xs font-semibold capitalize text-muted-ink">{value.replaceAll("_", " ")}</span>;
}

export function EmptyState({ children, title }: Readonly<{ children: ReactNode; title: string }>) {
  return (
    <section className="rounded-panel border bg-panel p-6 shadow-sm">
      <h2 className="font-semibold">{title}</h2>
      <div className="mt-2 text-sm leading-6 text-muted-ink">{children}</div>
    </section>
  );
}

export function ResourceBoundary<T>({ children, state }: Readonly<{
  children: (value: T) => ReactNode;
  state: ResourceState<T>;
}>) {
  if (state.status === "ready") return children(state.value);
  const messages = {
    loading: ["Loading authoritative data", "Steward is reading the current server-owned record."],
    "not-found": ["Not found", "The requested record does not exist in your server-authorized scope."],
    forbidden: ["Forbidden", "The Rust authorization boundary did not permit this request."],
    unavailable: ["Authoritative data unavailable", "The source of truth could not be reached. No placeholder data is shown."],
    error: ["Data could not be accepted", "Steward received an unexpected response and has not inferred a successful state."],
  } as const;
  const [title, detail] = messages[state.status];
  return <EmptyState title={title}><p role="status">{detail}</p></EmptyState>;
}

export function DefinitionList({ items }: Readonly<{ items: ReadonlyArray<readonly [string, ReactNode]> }>) {
  return (
    <dl className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
      {items.map(([term, value]) => (
        <div className="min-w-0" key={term}>
          <dt className="text-xs font-semibold uppercase tracking-wide text-muted-ink">{term}</dt>
          <dd className="mt-1 break-words text-sm">{value ?? "Not reported"}</dd>
        </div>
      ))}
    </dl>
  );
}
