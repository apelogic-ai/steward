import { AdminWorkflowDetailView } from "@/components/workflow-views";

export default async function AdminWorkflowVersionPage({ params }: Readonly<{ params: Promise<{ name: string; version: string }> }>) {
  const { name, version } = await params;
  return <AdminWorkflowDetailView name={name} version={Number(version)} />;
}
