import type { Metadata } from "next";
import { BrowserRunNowView } from "@/components/browser-run-now-view";

export const metadata: Metadata = { title: "Run now" };

export default function RunNowPage() {
  return <BrowserRunNowView />;
}
