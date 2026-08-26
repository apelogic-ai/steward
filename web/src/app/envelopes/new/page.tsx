import type { Metadata } from "next";
import { NewEnvelopeView } from "@/components/envelope-views";

export const metadata: Metadata = { title: "New envelope" };

export default function NewEnvelopePage() {
  return <NewEnvelopeView />;
}
