import type { ReactNode } from "react";

import { cn } from "@/lib/utils";

export function SectionCard({ actions, children, className, footer, title }: Readonly<{
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
  footer?: ReactNode;
  title?: ReactNode;
}>) {
  return (
    <section className={cn("overflow-hidden rounded-card border border-line bg-panel", className)}>
      {title || actions ? (
        <header className="flex min-h-12 items-center justify-between gap-4 border-b border-line-soft px-5 py-3.5">
          {title ? <h2 className="text-[15px] font-semibold leading-[22px]">{title}</h2> : <span />}
          {actions}
        </header>
      ) : null}
      <div className="px-5 py-4">{children}</div>
      {footer ? <footer className="border-t border-line-soft bg-subtle px-5 py-3.5">{footer}</footer> : null}
    </section>
  );
}
