import type { Metadata, Viewport } from "next";
import { Inter } from "next/font/google";

import { SITE_OPEN_GRAPH, SITE_TWITTER } from "@/lib/metadata";
import { SITE_DESCRIPTION, SITE_NAME, SITE_TITLE, SITE_URL } from "@/lib/site";
import { cn } from "@/lib/utils";

import "./globals.css";

// Inter (SIL OFL 1.1): the open face closest to the app's platform UI font; the
// opsz axis gives display sizes their own cut. See DESIGN.md.
const inter = Inter({ subsets: ["latin", "latin-ext"], axes: ["opsz"], variable: "--font-sans" });

// Runs before first paint: marks that scripts run, so CSS shows the animated tour
// instead of its static scenes (components/landing/tour.module.css).
const JS_FLAG = "document.documentElement.classList.add('js')";

export const metadata: Metadata = {
  metadataBase: new URL(SITE_URL),
  title: {
    default: SITE_TITLE,
    template: `%s | ${SITE_NAME}`,
  },
  description: SITE_DESCRIPTION,
  applicationName: SITE_NAME,
  authors: [{ name: "Lukáš Poddaný" }],
  creator: "Lukáš Poddaný",
  keywords: [
    "Cider",
    "Cider offline",
    "Apple Music offline",
    "Apple Music proxy",
    "Cider music client",
    "offline lyrics",
  ],
  openGraph: SITE_OPEN_GRAPH,
  twitter: SITE_TWITTER,
};

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
  themeColor: [
    { media: "(prefers-color-scheme: light)", color: "#ffffff" },
    { media: "(prefers-color-scheme: dark)", color: "#1c1c1e" },
  ],
};

export default function RootLayout({ children }: LayoutProps<"/">) {
  return (
    // suppressHydrationWarning: JS_FLAG changes the class before React hydrates.
    <html lang="en" className={cn("h-full antialiased font-sans", inter.variable)} suppressHydrationWarning>
      <head>
        <script dangerouslySetInnerHTML={{ __html: JS_FLAG }} />
      </head>
      <body className="flex min-h-full flex-col">{children}</body>
    </html>
  );
}
