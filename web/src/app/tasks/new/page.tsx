import type { Metadata } from "next";

import { TaskEditorView } from "@/components/task-library-view";

export const metadata: Metadata = { title: "New Task" };

export default function NewTaskPage() {
  return <TaskEditorView />;
}
