import type { Metadata } from "next";
import { Inconsolata, Space_Grotesk } from "next/font/google";
import { headers } from "next/headers";
import type { ReactNode } from "react";

import { AppShell } from "@/components/app-shell";
import { SessionProvider } from "@/session/session-context";

import "./globals.css";

const spaceGrotesk = Space_Grotesk({
  subsets: ["latin"],
  variable: "--font-space-grotesk",
});

const inconsolata = Inconsolata({
  subsets: ["latin"],
  variable: "--font-inconsolata",
});

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
    <html className={`${spaceGrotesk.variable} ${inconsolata.variable}`} lang="en">
      <body>
        <SessionProvider>
          <AppShell>{children}</AppShell>
        </SessionProvider>
      </body>
    </html>
  );
}
