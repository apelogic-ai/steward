import Link from "next/link";
import type { ReactNode } from "react";

import type { ResourceState } from "@/data/use-api-resource";
import { StatusPill } from "@/components/hs/status-pill";

export function PageHeader({ actions, description, title }: Readonly<{
  actions?: ReactNode;
  description: string;
  title: ReactNode;
}>) {
  return (
    <header className="flex flex-wrap items-end justify-between gap-4">
      <div className="max-w-3xl">
        <h1 className="text-[28px] font-semibold leading-[34px] tracking-[-0.02em]" id="page-title">{title}</h1>
        <p className="mt-1.5 text-sm leading-5 text-muted-ink">{description}</p>
      </div>
      {actions}
    </header>
  );
}

export function PrimaryLink({ children, href }: Readonly<{ children: ReactNode; href: string }>) {
  return <Link className="inline-flex h-10 items-center rounded-control bg-brand px-4 text-sm font-semibold text-on-brand hover:bg-brand-hover" href={href}>{children}</Link>;
}

export function StatusBadge({ value }: Readonly<{ value: string }>) {
  return <StatusPill value={value} />;
}

export function EmptyState({ children, title }: Readonly<{ children?: ReactNode; title: string }>) {
  return (
    <section className="rounded-panel border bg-panel p-6 shadow-sm">
      <h2 className="font-semibold">{title}</h2>
      {children ? <div className="mt-2 text-sm leading-6 text-muted-ink">{children}</div> : null}
    </section>
  );
}

export function ResourceBoundary<T>({ children, state }: Readonly<{
  children: (value: T) => ReactNode;
  state: ResourceState<T>;
}>) {
  if (state.status === "ready") return children(state.value);
  const messages = {
    loading: ["Loading…", "HyperShell is loading the latest information."],
    "not-found": ["Not found", "You don't have access to this item, or it no longer exists."],
    forbidden: ["Access denied", "You don't have permission to view this page."],
    unavailable: ["Couldn't load this page", "HyperShell couldn't reach the server. Try again in a moment."],
    error: ["Couldn't load this page", "HyperShell received an unexpected response. Reload the page or try again later."],
  } as const;
  const [title, detail] = messages[state.status];
  return <EmptyState title={title}><p role="status">{detail}</p></EmptyState>;
}

export function DefinitionList({ items }: Readonly<{ items: ReadonlyArray<readonly [string, ReactNode]> }>) {
  return (
    <dl className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
      {items.map(([term, value]) => (
        <div className="min-w-0" key={term}>
          <dt className="text-xs font-semibold text-muted-ink">{term}</dt>
          <dd className="mt-1 break-words text-sm">{value ?? "Not reported"}</dd>
        </div>
      ))}
    </dl>
  );
}
