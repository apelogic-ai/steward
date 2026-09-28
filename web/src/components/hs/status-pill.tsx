import { cn } from "@/lib/utils";

import { displayStatus, toneForStatus, type StatusTone } from "./tone";

const toneClasses: Record<StatusTone, string> = {
  ok: "bg-ok-soft text-ok",
  err: "bg-err-soft text-err",
  warn: "bg-warn-soft text-warn",
  info: "bg-info-soft text-info",
  neutral: "bg-line-soft text-muted-ink",
};

export function StatusPill({ className, tone, value }: Readonly<{
  className?: string;
  tone?: StatusTone;
  value: string;
}>) {
  const resolvedTone = tone ?? toneForStatus(value);
  return (
    <span
      className={cn("inline-flex w-fit shrink-0 items-center gap-1.5 self-start rounded-full px-2.5 py-[3px] text-xs font-semibold capitalize", toneClasses[resolvedTone], className)}
      data-tone={resolvedTone}
    >
      <span aria-hidden="true" className="size-1.5 rounded-full bg-current" />
      {displayStatus(value)}
    </span>
  );
}
