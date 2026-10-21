import type { Metadata } from "next";

import { PersonalAgentDetailView } from "@/components/personal-agent-views";

export const metadata: Metadata = { title: "Agent" };

export default async function PersonalAgentPage({ params }: Readonly<{ params: Promise<{ id: string }> }>) {
  const { id } = await params;
  return <PersonalAgentDetailView agentId={id} />;
}
