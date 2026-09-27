"use client";

import { useState } from "react";

function highlightedYamlLine(line: string) {
    const match = /^(\s*-?\s*)([^:#]+:)(.*)$/.exec(line);
    if (!match) return line;
    const [, prefix, key, rawValue] = match;
    const value = rawValue.trim();
    const valueClass = value === "true" || value === "false"
      ? "text-code-bool"
      : /^[A-Za-z_-]+$/.test(value)
        ? "text-code-ident"
        : "text-code-string";
    return <>{prefix}<span className="text-code-key">{key}</span>{rawValue ? <span className={valueClass}>{rawValue}</span> : null}</>;
}

export function CodeBlock({ code, language = "text", path }: Readonly<{
  code: string;
  language?: "yaml" | "shell" | "text";
  path?: string;
}>) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    await navigator.clipboard.writeText(code);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1500);
  };
  return (
    <section className="overflow-hidden rounded-tile border border-code-line bg-code text-code-ink">
      <header className="flex min-h-10 items-center justify-between gap-4 border-b border-code-line px-4 py-2 text-xs text-code-muted">
        <span className="truncate font-mono">{path ?? language}</span>
        <button className="rounded-control border border-code-line px-2.5 py-1 font-sans font-semibold text-code-ink hover:bg-white/10" onClick={() => void copy()} type="button">
          {copied ? "Copied" : "Copy"}
        </button>
      </header>
      <pre className="overflow-x-auto py-4 font-mono text-[13px] leading-5"><code>{code.split("\n").map((line, index) => <span className="grid grid-cols-[3rem_minmax(max-content,1fr)] px-4" key={`${index}-${line}`}><span aria-hidden="true" className="select-none border-r border-code-line pr-3 text-right text-code-muted">{index + 1}</span><span className="pl-4">{language === "yaml" ? highlightedYamlLine(line) : line || " "}</span></span>)}</code></pre>
    </section>
  );
}
