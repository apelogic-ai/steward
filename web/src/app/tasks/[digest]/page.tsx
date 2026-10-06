import type { Metadata } from "next";

import { TaskDetailView } from "@/components/task-view";

export const metadata: Metadata = { title: "Task" };

export default async function TaskPage({ params }: Readonly<{ params: Promise<{ digest: string }> }>) {
  const { digest } = await params;
  return <TaskDetailView contentDigest={decodeURIComponent(digest)} />;
}
