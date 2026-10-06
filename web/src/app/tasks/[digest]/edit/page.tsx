import type { Metadata } from "next";

import { TaskEditorView } from "@/components/task-library-view";

export const metadata: Metadata = { title: "Edit Task" };

export default async function EditTaskPage({ params }: Readonly<{ params: Promise<{ digest: string }> }>) {
  const { digest } = await params;
  return <TaskEditorView contentDigest={decodeURIComponent(digest)} />;
}
