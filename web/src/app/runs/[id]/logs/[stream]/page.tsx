import type { Metadata } from "next";
import { notFound } from "next/navigation";

import { RunLogView } from "@/components/run-log-view";
import { isExecutionLogStream } from "@/data/execution-log";

export const metadata: Metadata = { title: "Run log" };

export default async function RunLogPage({
  params,
}: Readonly<{ params: Promise<{ id: string; stream: string }> }>) {
  const { id, stream } = await params;
  if (!isExecutionLogStream(stream)) notFound();
  return <RunLogView stream={stream} taskUid={id} />;
}
