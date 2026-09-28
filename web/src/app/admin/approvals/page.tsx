import type { Metadata } from "next";
import { AdminApprovalsView } from "@/components/admin-approvals-view";

export const metadata: Metadata = { title: "Requests" };

export default function AdminApprovalsPage() {
  return <AdminApprovalsView />;
}
