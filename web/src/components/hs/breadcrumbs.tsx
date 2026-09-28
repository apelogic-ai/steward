import Link from "next/link";

import { cn } from "@/lib/utils";

export type BreadcrumbItem = {
  href?: string;
  label: string;
  mono?: boolean;
};

export function Breadcrumbs({ items }: Readonly<{ items: readonly BreadcrumbItem[] }>) {
  return (
    <nav aria-label="Breadcrumb" className="mb-4 text-[13px] leading-[19px]">
      <ol className="flex flex-wrap items-center">
        {items.map((item, index) => {
          const current = index === items.length - 1;
          const className = cn(
            item.mono && "font-mono",
            current ? "font-semibold text-link" : index === 0 ? "text-faint-ink" : "text-muted-ink",
          );
          return (
            <li className="flex items-center" key={`${item.href ?? "current"}:${item.label}`}>
              {index > 0 ? <span aria-hidden="true" className="px-1 text-faint-ink">/</span> : null}
              {item.href ? <Link className={cn(className, "hover:text-ink hover:underline hover:underline-offset-[3px]")} href={item.href}>{item.label}</Link> : <span aria-current={current ? "page" : undefined} className={className}>{item.label}</span>}
            </li>
          );
        })}
      </ol>
    </nav>
  );
}
