import type { Metadata } from "next";

import { AdminRequestDetailView } from "@/components/admin-approvals-view";

export const metadata: Metadata = { title: "Request" };

export default async function AdminRequestDetailPage({ params }: Readonly<{ params: Promise<{ id: string }> }>) {
  const { id } = await params;
  return <AdminRequestDetailView requestId={id} />;
}
