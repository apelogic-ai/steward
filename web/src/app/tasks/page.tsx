import type { Metadata } from "next";

import { TaskLibraryView } from "@/components/task-library-view";

export const metadata: Metadata = { title: "Tasks" };

export default function TasksPage() {
  return <TaskLibraryView />;
}
