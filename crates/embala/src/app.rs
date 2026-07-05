//! macOS `.app` bundle builder.
//!
//! Lays out `<display-name>.app` as a plain directory via `apple-bundles`:
//! `Contents/Info.plist`, the executable under `Contents/MacOS/<name>`, and
//! an `.icns` rendered from the configured PNG under `Contents/Resources/`.
//! The bundle is unsigned (signing/notarization is v2 scope) and the emitted
//! file contents are deterministic — no wall-clock input anywhere.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use apple_bundles::MacOsApplicationBundleBuilder;
use image::imageops::FilterType;
use simple_file_manifest::FileEntry;
use tauri_icns::{IconFamily, IconType, Image, PixelFormat};

use crate::config::{AppSection, Package};

/// The RGBA icns entries a modern app icon carries: 16–512 points plus the
/// @2x retina variants where the format defines one (64pt has no @2x type).
const ICON_TYPES: [IconType; 11] = [
    IconType::RGBA32_16x16,
    IconType::RGBA32_16x16_2x,
    IconType::RGBA32_32x32,
    IconType::RGBA32_32x32_2x,
    IconType::RGBA32_64x64,
    IconType::RGBA32_128x128,
    IconType::RGBA32_128x128_2x,
    IconType::RGBA32_256x256,
    IconType::RGBA32_256x256_2x,
    IconType::RGBA32_512x512,
    IconType::RGBA32_512x512_2x,
];

/// Build `<out_dir>/<display-name>.app`. `base` is the directory the config
/// file lives in; the section's paths are resolved against it. An existing
/// bundle directory is replaced wholesale so rebuilds never accrete stale
/// files.
pub fn build(
    package: &Package,
    section: &AppSection,
    base: &Path,
    out_dir: &Path,
) -> Result<PathBuf> {
    // new() also pins CFBundlePackageType = "APPL".
    let mut builder = MacOsApplicationBundleBuilder::new(&package.display_name)?;
    builder.set_info_plist_required_keys(
        &package.display_name,
        &package.identifier,
        &package.version,
        "????", // unregistered creator code — the modern convention
        &package.name,
    )?;
    builder.set_info_plist_key("CFBundleShortVersionString", package.version.as_str())?;
    if let Some(min) = &section.minimum_system_version {
        builder.set_info_plist_key("LSMinimumSystemVersion", min.as_str())?;
    }

    if let Some(icon) = &section.icon {
        let icon_path = base.join(icon);
        let png = std::fs::read(&icon_path)
            .with_context(|| format!("reading app icon {}", icon_path.display()))?;
        let icns = render_icns(&png)
            .with_context(|| format!("rendering icns from {}", icon_path.display()))?;
        // Lands at Contents/Resources/<CFBundleName>.icns.
        builder.add_icon(icns)?;
        builder.set_info_plist_key("CFBundleIconFile", format!("{}.icns", package.display_name))?;
    }

    let exe_path = base.join(&section.executable);
    let exe = std::fs::read(&exe_path)
        .with_context(|| format!("reading app executable {}", exe_path.display()))?;
    builder.add_file_macos(&package.name, FileEntry::new_from_data(exe, true))?;

    std::fs::create_dir_all(out_dir)?;
    let bundle_dir = out_dir.join(format!("{}.app", package.display_name));
    if bundle_dir.exists() {
        std::fs::remove_dir_all(&bundle_dir)
            .with_context(|| format!("removing stale bundle {}", bundle_dir.display()))?;
    }
    builder.materialize_bundle(out_dir)
}

/// Decode a PNG and encode the standard icns size set from it.
fn render_icns(png: &[u8]) -> Result<Vec<u8>> {
    let source = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .context("decoding PNG")?;
    let mut family = IconFamily::new();
    for icon_type in ICON_TYPES {
        let (w, h) = (icon_type.pixel_width(), icon_type.pixel_height());
        let resized = source.resize_exact(w, h, FilterType::Lanczos3).into_rgba8();
        let icon = Image::from_data(PixelFormat::RGBA, w, h, resized.into_raw())?;
        family.add_icon_with_type(&icon, icon_type)?;
    }
    let mut out = Vec::new();
    family.write(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/hello"))
    }

    fn package() -> Package {
        Package {
            name: "hello".to_string(),
            display_name: "Embala Hello".to_string(),
            version: "0.1.0".to_string(),
            identifier: "ca.hrndz.embala.hello".to_string(),
            publisher: "Carlos Hernandez".to_string(),
            description: "Embala end-to-end test fixture".to_string(),
            homepage: None,
            license: None,
        }
    }

    fn section() -> AppSection {
        AppSection {
            executable: PathBuf::from("hello.sh"),
            icon: Some(PathBuf::from("icon.png")),
            minimum_system_version: None,
        }
    }

    /// Fresh per-test output directory under the system temp dir (unit tests
    /// have no CARGO_TARGET_TMPDIR).
    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("embala-app-{}-{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn build_fixture(out_dir: &Path) -> PathBuf {
        build(&package(), &section(), &fixture_dir(), out_dir).expect("bundle builds")
    }

    fn plist_dict(bundle: &Path) -> plist::Dictionary {
        plist::Value::from_file(bundle.join("Contents/Info.plist"))
            .expect("Info.plist parses")
            .into_dictionary()
            .expect("Info.plist is a dictionary")
    }

    #[test]
    fn bundle_has_the_expected_layout() {
        let out = tmp("layout");
        let bundle = build_fixture(&out);
        assert_eq!(bundle, out.join("Embala Hello.app"));
        assert!(bundle.join("Contents/Info.plist").is_file());
        assert!(bundle.join("Contents/MacOS/hello").is_file());
        assert!(
            bundle
                .join("Contents/Resources/Embala Hello.icns")
                .is_file()
        );
    }

    #[test]
    fn info_plist_carries_the_bundle_metadata() {
        let out = tmp("plist");
        let dict = plist_dict(&build_fixture(&out));
        let expect = [
            ("CFBundleName", "Embala Hello"),
            ("CFBundleIdentifier", "ca.hrndz.embala.hello"),
            ("CFBundleVersion", "0.1.0"),
            ("CFBundleSignature", "????"),
            ("CFBundleExecutable", "hello"),
            ("CFBundlePackageType", "APPL"),
            ("CFBundleShortVersionString", "0.1.0"),
            ("CFBundleIconFile", "Embala Hello.icns"),
        ];
        for (key, value) in expect {
            assert_eq!(
                dict.get(key).and_then(|v| v.as_string()),
                Some(value),
                "key {key}"
            );
        }
        // The fixture doesn't configure a minimum system version.
        assert!(!dict.contains_key("LSMinimumSystemVersion"));
    }

    #[test]
    fn minimum_system_version_is_emitted_when_configured() {
        let out = tmp("minver");
        let section = AppSection {
            minimum_system_version: Some("11.0".to_string()),
            ..section()
        };
        let bundle = build(&package(), &section, &fixture_dir(), &out).expect("bundle builds");
        let dict = plist_dict(&bundle);
        assert_eq!(
            dict.get("LSMinimumSystemVersion")
                .and_then(|v| v.as_string()),
            Some("11.0")
        );
    }

    #[cfg(unix)]
    #[test]
    fn bundle_executable_keeps_the_exec_bit() {
        use std::os::unix::fs::PermissionsExt as _;
        let out = tmp("mode");
        let bundle = build_fixture(&out);
        let mode = std::fs::metadata(bundle.join("Contents/MacOS/hello"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "exec bit missing (mode {mode:o})");
    }

    #[test]
    fn icns_is_nonempty_and_carries_the_magic() {
        let out = tmp("icns");
        let bundle = build_fixture(&out);
        let icns = std::fs::read(bundle.join("Contents/Resources/Embala Hello.icns")).unwrap();
        assert!(icns.len() > 8);
        assert_eq!(&icns[..4], b"icns");
    }

    #[test]
    fn builds_are_deterministic_at_the_file_level() {
        let out_a = tmp("det-a");
        let out_b = tmp("det-b");
        let a = build_fixture(&out_a);
        let b = build_fixture(&out_b);
        for rel in [
            "Contents/Info.plist",
            "Contents/Resources/Embala Hello.icns",
        ] {
            assert_eq!(
                std::fs::read(a.join(rel)).unwrap(),
                std::fs::read(b.join(rel)).unwrap(),
                "{rel} differs between builds"
            );
        }
    }
}
