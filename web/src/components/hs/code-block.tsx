"use client";

import { useState } from "react";

function highlightedYaml(code: string) {
  return code.split("\n").map((line, index) => {
    const match = /^(\s*-?\s*)([^:#]+:)(.*)$/.exec(line);
    if (!match) return <span key={index}>{line}{index < code.split("\n").length - 1 ? "\n" : ""}</span>;
    const [, prefix, key, rawValue] = match;
    const value = rawValue.trim();
    const valueClass = value === "true" || value === "false"
      ? "text-code-bool"
      : /^[A-Za-z_-]+$/.test(value)
        ? "text-code-ident"
        : "text-code-string";
    return <span key={index}>{prefix}<span className="text-code-key">{key}</span>{rawValue ? <span className={valueClass}>{rawValue}</span> : null}{index < code.split("\n").length - 1 ? "\n" : ""}</span>;
  });
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
    <section className="overflow-hidden rounded-card border border-code-line bg-code text-code-ink">
      <header className="flex min-h-10 items-center justify-between gap-4 border-b border-code-line px-4 py-2 text-xs text-code-muted">
        <span className="truncate font-mono">{path ?? language}</span>
        <button className="rounded-control px-2.5 py-1 font-sans font-semibold text-code-ink hover:bg-white/10" onClick={() => void copy()} type="button">
          {copied ? "Copied" : "Copy"}
        </button>
      </header>
      <pre className="overflow-x-auto p-4 font-mono text-sm leading-[21px]"><code>{language === "yaml" ? highlightedYaml(code) : code}</code></pre>
    </section>
  );
}
