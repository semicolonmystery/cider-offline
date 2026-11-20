import type { Metadata } from "next";

import { Landing } from "@/components/landing/landing";
import { SITE_OPEN_GRAPH } from "@/lib/metadata";

// The landing page: a scroll-driven tour of the app (see DESIGN.md).
export const metadata: Metadata = {
  alternates: { canonical: "/" },
  openGraph: { ...SITE_OPEN_GRAPH, url: "/" },
};

export default function Home() {
  return <Landing />;
}
