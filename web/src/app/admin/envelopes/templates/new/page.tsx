import type { Metadata } from "next";

import { AdminNewEnvelopeTemplateView } from "@/components/admin-template-view";

export const metadata: Metadata = { title: "Create envelope template" };

export default function AdminNewEnvelopeTemplatePage() {
  return <AdminNewEnvelopeTemplateView />;
}
