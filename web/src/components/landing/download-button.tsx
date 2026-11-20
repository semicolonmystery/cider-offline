"use client";

// The hero's Download button. The server renders a plain "Download"; in the browser
// the label names the visitor's system. Every system's files are on the same
// release page, so the link never changes.

import { useSyncExternalStore } from "react";

import { buttonVariants } from "@/components/ui/button";
import { RELEASES_URL } from "@/lib/site";
import { cn } from "@/lib/utils";

type Os = "Windows" | "macOS" | "Linux";

function detectOs(): Os | null {
  const nav = navigator as Navigator & { userAgentData?: { platform?: string } };
  const platform = (nav.userAgentData?.platform || nav.userAgent).toLowerCase();
  // Phones and tablets get the plain label: the app is for desktops.
  if (/android|iphone|ipad|ipod/.test(nav.userAgent.toLowerCase())) return null;
  if (platform.includes("win")) return "Windows";
  if (platform.includes("mac")) return "macOS";
  if (platform.includes("linux") || platform.includes("x11")) return "Linux";
  return null;
}

const noop = () => () => {};

export function DownloadButton({ className }: { className?: string }) {
  const os = useSyncExternalStore(noop, detectOs, () => null);
  return (
    <a
      href={RELEASES_URL}
      className={cn(
        buttonVariants({ size: "lg" }),
        "h-12 rounded-[10px] bg-accent-strong px-6 text-base font-semibold text-white hover:bg-accent-strong-hover",
        className,
      )}
    >
      {os ? `Download for ${os}` : "Download"}
    </a>
  );
}
