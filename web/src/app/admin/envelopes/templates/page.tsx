import type { Metadata } from "next";
import { AdminEnvelopeTemplatesView } from "@/components/admin-template-view";

export const metadata: Metadata = { title: "Envelope templates" };

export default function AdminEnvelopeTemplatesPage() {
  return <AdminEnvelopeTemplatesView />;
}
