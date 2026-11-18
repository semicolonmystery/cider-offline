import { readFile } from "node:fs/promises";
import { join } from "node:path";

import { ImageResponse } from "next/og";

import { OG_IMAGE, SITE_HOST, SITE_NAME, SITE_TAGLINE } from "@/lib/site";

// The Open Graph and Twitter image, rendered once at build time to out/og.png.
// A route handler instead of the opengraph-image.tsx convention: in a static export
// that convention writes an extensionless `opengraph-image` file, which GitHub Pages
// would serve as application/octet-stream instead of image/png.
export const dynamic = "force-static";

// The app's dark palette (crates/app/ui/components.slint).
const BACKGROUND = "#1C1C1E";
const TEXT = "#F5F5F7";
const TEXT_SECONDARY = "#98989D";
const ACCENT = "#FA2D48";

// The generated app icon. `next build` runs with web/ as the working directory.
const ICON_PATH = join(process.cwd(), "..", "packaging", "icons", "512x512.png");
// Geist, the site font, from the `geist` package (next/og only bundles the regular weight).
const FONT_DIR = join(process.cwd(), "node_modules", "geist", "dist", "fonts", "geist-sans");

export async function GET() {
  const [icon, regular, semiBold] = await Promise.all([
    readFile(ICON_PATH, "base64"),
    readFile(join(FONT_DIR, "Geist-Regular.ttf")),
    readFile(join(FONT_DIR, "Geist-SemiBold.ttf")),
  ]);

  return new ImageResponse(
    (
      <div
        style={{
          width: "100%",
          height: "100%",
          display: "flex",
          flexDirection: "column",
          justifyContent: "center",
          padding: "0 96px",
          background: BACKGROUND,
          color: TEXT,
          fontFamily: "Geist",
          borderTop: `12px solid ${ACCENT}`,
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: 48 }}>
          {/* eslint-disable-next-line @next/next/no-img-element -- Satori renders plain <img>. */}
          <img src={`data:image/png;base64,${icon}`} width={200} height={200} alt="" />
          <div style={{ display: "flex", flexDirection: "column", gap: 20 }}>
            <div style={{ fontSize: 88, fontWeight: 600, lineHeight: 1 }}>{SITE_NAME}</div>
            <div style={{ fontSize: 38, color: TEXT_SECONDARY, maxWidth: 720 }}>
              {SITE_TAGLINE}
            </div>
          </div>
        </div>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 16,
            marginTop: 72,
            fontSize: 32,
            color: TEXT_SECONDARY,
          }}
        >
          <div style={{ width: 16, height: 16, borderRadius: 8, background: ACCENT }} />
          {SITE_HOST}
        </div>
      </div>
    ),
    {
      width: OG_IMAGE.width,
      height: OG_IMAGE.height,
      fonts: [
        { name: "Geist", data: regular, weight: 400, style: "normal" },
        { name: "Geist", data: semiBold, weight: 600, style: "normal" },
      ],
    },
  );
}
