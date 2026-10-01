import type { Metadata } from "next";

import { AdminSetupView } from "@/components/admin-setup-view";

export const metadata: Metadata = { title: "Admin get started" };

export default function AdminGetStartedPage() {
  return <AdminSetupView />;
}
