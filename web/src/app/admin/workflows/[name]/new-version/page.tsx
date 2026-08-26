import { NewWorkflowVersionView } from "@/components/workflow-views";

export default async function NewWorkflowVersionPage({ params }: Readonly<{ params: Promise<{ name: string }> }>) {
  const { name } = await params;
  return <NewWorkflowVersionView name={name} />;
}
