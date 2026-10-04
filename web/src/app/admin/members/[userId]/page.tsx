import type { Metadata } from "next";

import { AdminMemberDetailView } from "@/components/admin-member-detail-view";

export const metadata: Metadata = { title: "Member" };

export default async function AdminMemberDetailPage({ params }: Readonly<{ params: Promise<{ userId: string }> }>) {
  const { userId } = await params;
  return <AdminMemberDetailView userId={userId} />;
}
