import type { Metadata } from "next";
import { EnvelopesView } from "@/components/envelope-views";

export const metadata: Metadata = { title: "Envelopes" };

export default function EnvelopesPage() {
  return <EnvelopesView />;
}
