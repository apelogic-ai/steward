import type { Metadata } from "next";
import { headers } from "next/headers";
import type { ReactNode } from "react";

import { AppShell } from "@/components/app-shell";
import { SessionProvider } from "@/session/session-context";

import "./globals.css";

export const metadata: Metadata = {
  title: {
    default: "Steward",
    template: "%s · Steward",
  },
  description: "Governed agent runtimes and authority envelopes.",
};

export default async function RootLayout({ children }: Readonly<{ children: ReactNode }>) {
  await headers();
  return (
    <html lang="en">
      <body>
        <SessionProvider>
          <AppShell>{children}</AppShell>
        </SessionProvider>
      </body>
    </html>
  );
}
