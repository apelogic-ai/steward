import type { Metadata } from "next";

import { AdminEnvelopeTemplateDetailView } from "@/components/admin-template-view";

export const metadata: Metadata = { title: "Envelope template" };

export default async function AdminEnvelopeTemplatePage({ params }: Readonly<{
  params: Promise<{ templateId: string }>;
}>) {
  const { templateId } = await params;
  return <AdminEnvelopeTemplateDetailView memberRole={templateId} />;
}
