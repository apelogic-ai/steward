import type { Metadata } from "next";
import { RunDetailView } from "@/components/run-views";

export const metadata: Metadata = { title: "Run" };

export default async function RunPage({ params }: Readonly<{ params: Promise<{ id: string }> }>) {
  const { id } = await params;
  return <RunDetailView taskUid={id} />;
}
