import type { Metadata } from "next";

import { NewPersonalAgentView } from "@/components/personal-agent-views";

export const metadata: Metadata = { title: "New agent" };

export default function NewPersonalAgentPage() {
  return <NewPersonalAgentView />;
}
