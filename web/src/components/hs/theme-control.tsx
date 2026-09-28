"use client";

import { useEffect, useState } from "react";

import { getBrowserPreferences, updateBrowserPreferences, type BrowserTheme } from "@/api-client";
import { useSession } from "@/session/session-context";

function applyTheme(theme: BrowserTheme) {
  if (theme === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = theme;
  document.cookie = `hypershell-theme=${theme}; Path=/; Max-Age=31536000; SameSite=Lax`;
}

export function ThemeControl() {
  const session = useSession();
  const [theme, setTheme] = useState<BrowserTheme>("system");
  useEffect(() => {
    let active = true;
    void getBrowserPreferences({ cache: "no-store", credentials: "same-origin" }).then((result) => {
      if (!active || !result.data || !result.response?.ok) return;
      setTheme(result.data.theme ?? "system");
    });
    return () => { active = false; };
  }, []);
  const select = (next: BrowserTheme) => {
    if (session.status !== "authenticated") return;
    setTheme(next);
    applyTheme(next);
    void updateBrowserPreferences({ body: { theme: next }, cache: "no-store", credentials: "same-origin", headers: { "X-Steward-CSRF": session.value.csrf } });
  };
  return <div aria-label="Colour mode" className="grid grid-cols-3 rounded-control bg-line-soft p-[3px]" role="group">{(["system", "light", "dark"] as const).map((option) => <button aria-pressed={theme === option} className={`h-8 rounded-[6px] px-3 text-xs font-semibold capitalize ${theme === option ? "bg-panel text-ink shadow-control" : "text-muted-ink hover:text-ink"}`} key={option} onClick={() => select(option)} type="button">{option}</button>)}</div>;
}
