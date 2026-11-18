//! Rasterises the icon sources in `packaging/icons/src/` into every committed
//! icon asset (package PNGs, the multi-size Windows `.ico`, the window icon and
//! the tray icons). See `packaging/README.md`.

use std::fs;
use std::path::Path;

use anyhow::Context as _;
use image::ExtendedColorType;
use image::codecs::ico::{IcoEncoder, IcoFrame};
use resvg::{tiny_skia, usvg};

/// Sizes in the Windows `.ico` (16 comes from the hand-hinted master).
const ICO_SIZES: &[u32] = &[16, 20, 24, 32, 40, 48, 64, 128, 256];
/// Package PNGs listed in `crates/app/Packager.toml` (`.icns` + Linux hicolor).
/// Only sizes that map to an `.icns` icon type: cargo-packager rejects others.
const PACKAGE_SIZES: &[u32] = &[16, 32, 128, 256, 512];
/// Tray icons; the OS scales them down to its small-icon size.
const TRAY_SIZE: u32 = 32;
/// macOS menu-bar template icon: 22 pt at 2x.
const TEMPLATE_SIZE: u32 = 44;
/// Opacity of the Passthrough template icon (a template only carries alpha,
/// so the mode is shown by fading the glyph).
const PASSTHROUGH_TEMPLATE_ALPHA: f32 = 0.5;
/// Window / sidebar icon used by the Slint UI.
const WINDOW_SIZE: u32 = 256;
/// Largest size that still uses the hand-hinted 16 px master.
const HINTED_MAX: u32 = 16;

/// The parsed sources: the main artwork and its pixel-hinted 16 px master.
struct Artwork {
    main: usvg::Tree,
    small: Option<usvg::Tree>,
}

impl Artwork {
    fn load(main: &Path, small: Option<&Path>) -> anyhow::Result<Self> {
        Ok(Self {
            main: load_svg(main)?,
            small: small.map(load_svg).transpose()?,
        })
    }

    /// Renders a square PNG of `size` px, picking the master for that size.
    fn png(&self, size: u32) -> anyhow::Result<Vec<u8>> {
        self.png_with_opacity(size, 1.0)
    }

    /// Like [`Self::png`], with every pixel's opacity scaled by `opacity`.
    fn png_with_opacity(&self, size: u32, opacity: f32) -> anyhow::Result<Vec<u8>> {
        let tree = match &self.small {
            Some(small) if size <= HINTED_MAX => small,
            _ => &self.main,
        };
        let mut pixmap = tiny_skia::Pixmap::new(size, size).context("zero-sized icon")?;
        // Icon sizes are small integers, exactly representable as f32.
        #[expect(clippy::cast_precision_loss, reason = "icon sizes are <= 512")]
        let scale = size as f32 / tree.size().width();
        resvg::render(
            tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        if opacity < 1.0 {
            // Premultiplied RGBA: scaling all four channels scales the opacity
            // and keeps the colour.
            for channel in pixmap.data_mut() {
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "the product of a u8 and an opacity in [0, 1] fits a u8"
                )]
                let scaled = (f32::from(*channel) * opacity).round() as u8;
                *channel = scaled;
            }
        }
        // `encode_png` un-premultiplies the pixels.
        pixmap.encode_png().context("encoding PNG")
    }
}

fn load_svg(path: &Path) -> anyhow::Result<usvg::Tree> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    usvg::Tree::from_data(&data, &usvg::Options::default())
        .with_context(|| format!("parsing {}", path.display()))
}

fn write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

fn write_ico(art: &Artwork, path: &Path) -> anyhow::Result<()> {
    let pngs = ICO_SIZES
        .iter()
        .map(|&size| art.png(size).map(|png| (size, png)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let frames = pngs
        .iter()
        .map(|(size, png)| {
            IcoFrame::with_encoded(png.as_slice(), *size, *size, ExtendedColorType::Rgba8)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut ico = Vec::new();
    IcoEncoder::new(&mut ico).encode_images(&frames)?;
    write(path, &ico)
}

fn main() -> anyhow::Result<()> {
    // `absolute` resolves the `..`s without the `\\?\` prefix `canonicalize`
    // adds on Windows (it only shows up in the log lines).
    let root = std::path::absolute(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."))
        .context("locating the repository root")?;
    let icons = root.join("packaging").join("icons");
    let src = icons.join("src");
    let assets = root.join("crates").join("app").join("assets");

    let app = Artwork::load(&src.join("app.svg"), Some(&src.join("app-16.svg")))?;
    let passthrough = Artwork::load(&src.join("tray-passthrough.svg"), None)?;
    let template = Artwork::load(&src.join("tray-template.svg"), None)?;

    for &size in PACKAGE_SIZES {
        write(&icons.join(format!("{size}x{size}.png")), &app.png(size)?)?;
    }
    write_ico(&app, &icons.join("icon.ico"))?;
    write(&assets.join("icon-256.png"), &app.png(WINDOW_SIZE)?)?;
    write(&assets.join("tray-active.png"), &app.png(TRAY_SIZE)?)?;
    write(
        &assets.join("tray-passthrough.png"),
        &passthrough.png(TRAY_SIZE)?,
    )?;
    write(
        &assets.join("tray-template.png"),
        &template.png(TEMPLATE_SIZE)?,
    )?;
    write(
        &assets.join("tray-template-passthrough.png"),
        &template.png_with_opacity(TEMPLATE_SIZE, PASSTHROUGH_TEMPLATE_ALPHA)?,
    )?;
    Ok(())
}
