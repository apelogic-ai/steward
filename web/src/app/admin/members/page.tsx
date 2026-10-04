import type { Metadata } from "next";

import { AdminMembersView } from "@/components/admin-members-view";

export const metadata: Metadata = { title: "Members" };

export default function AdminMembersPage() {
  return <AdminMembersView />;
}
