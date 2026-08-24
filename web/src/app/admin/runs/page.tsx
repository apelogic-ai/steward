import type { Metadata } from "next";
import { RunsView } from "@/components/run-views";

export const metadata: Metadata = { title: "All runs" };

export default function AdminRunsPage() {
  return <RunsView admin />;
}
