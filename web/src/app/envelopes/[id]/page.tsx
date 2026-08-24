import type { Metadata } from "next";
import { EnvelopeDetailView } from "@/components/envelope-views";

export const metadata: Metadata = { title: "Envelope" };

export default async function EnvelopePage({ params }: Readonly<{ params: Promise<{ id: string }> }>) {
  const { id } = await params;
  return <EnvelopeDetailView requestId={id} />;
}
