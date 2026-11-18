//! Build script: compiles the Slint UI, converts the tray icons to raw RGBA
//! and, on Windows, embeds the exe icon resource.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

/// Tray icon PNGs in `assets/`, embedded as raw RGBA for `tray_icon::Icon::from_rgba`.
const TRAY_ICONS: &[(&str, &str)] = &[
    ("TRAY_ACTIVE", "assets/tray-active.png"),
    ("TRAY_PASSTHROUGH", "assets/tray-passthrough.png"),
];

/// macOS menu-bar icons: the glyph alone in black on transparent, drawn as a
/// template image so macOS tints it for light, dark and tinted menu bars.
const TRAY_TEMPLATE_ICONS: &[(&str, &str)] = &[
    ("TRAY_TEMPLATE", "assets/tray-template.png"),
    (
        "TRAY_TEMPLATE_PASSTHROUGH",
        "assets/tray-template-passthrough.png",
    ),
];

/// Multi-size app icon embedded into the Windows executable. Generated with
/// the other icon assets by `packaging/icons/gen` (see `packaging/README.md`).
#[cfg(windows)]
const EXE_ICON: &str = "../../packaging/icons/icon.ico";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    compile_ui()?;
    embed_tray_icons()?;
    embed_windows_resources()?;
    Ok(())
}

/// Embeds the exe icon (Explorer, taskbar, Alt+Tab) and the display name that
/// Task Manager shows. `winresource` is a Windows-host build dependency, so a
/// cross build from another OS ships without these resources.
#[cfg(windows)]
fn embed_windows_resources() -> Result<(), Box<dyn std::error::Error>> {
    if env::var("CARGO_CFG_TARGET_OS")? != "windows" {
        return Ok(());
    }
    println!("cargo::rerun-if-changed={EXE_ICON}");
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(EXE_ICON)
        .set("FileDescription", "Cider Offline")
        .set("ProductName", "Cider Offline");
    resource.compile()?;
    Ok(())
}

#[cfg(not(windows))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "same signature as the Windows variant"
)]
fn embed_windows_resources() -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

/// Picks the platform-appropriate std-widgets style. Light/dark is not fixed
/// here (no `-light`/`-dark` suffix), so the style follows the OS setting.
fn compile_ui() -> Result<(), Box<dyn std::error::Error>> {
    let style = match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => "cupertino",
        // Windows, and Linux without depending on Qt (which `native` would pick).
        _ => "fluent",
    };
    let config = slint_build::CompilerConfiguration::new().with_style(style.to_owned());
    slint_build::compile_with_config("ui/app.slint", config)?;
    Ok(())
}

fn embed_tray_icons() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    // Each target embeds only the set it shows, so neither is dead code.
    let icons = if env::var("CARGO_CFG_TARGET_OS")? == "macos" {
        TRAY_TEMPLATE_ICONS
    } else {
        TRAY_ICONS
    };
    let mut code = String::new();
    for (name, path) in icons {
        println!("cargo::rerun-if-changed={path}");
        let image = image::open(path)?.into_rgba8();
        let (width, height) = image.dimensions();
        let file = format!("{}.rgba", name.to_lowercase());
        fs::write(out_dir.join(&file), image.as_raw())?;
        writeln!(
            code,
            "/// Raw RGBA tray icon generated from `{path}`.\n\
             const {name}: RawIcon = RawIcon {{ \
             rgba: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{file}\")), \
             width: {width}, height: {height} }};"
        )?;
    }
    fs::write(out_dir.join("tray_icons.rs"), code)?;
    Ok(())
}
