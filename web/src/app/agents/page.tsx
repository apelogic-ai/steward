import type { Metadata } from "next";

import { PersonalAgentsView } from "@/components/personal-agent-views";

export const metadata: Metadata = { title: "My agents" };

export default function PersonalAgentsPage() {
  return <PersonalAgentsView />;
}
