import type { Metadata } from "next";
import { RunsView } from "@/components/run-views";

export const metadata: Metadata = { title: "Runs" };

export default function RunsPage() {
  return <RunsView />;
}
