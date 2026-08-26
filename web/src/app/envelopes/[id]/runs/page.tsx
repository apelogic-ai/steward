import type { Metadata } from "next";
import { EnvelopeRunsView } from "@/components/envelope-views";

export const metadata: Metadata = { title: "Envelope runs" };

export default async function EnvelopeRunsPage({ params }: Readonly<{ params: Promise<{ id: string }> }>) {
  const { id } = await params;
  return <EnvelopeRunsView requestId={id} />;
}
