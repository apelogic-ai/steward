import type { Metadata } from "next";
import { SettingsView } from "@/components/settings-view";

export const metadata: Metadata = { title: "Admin settings" };

export default function AdminSettingsPage() {
  return <SettingsView admin />;
}
