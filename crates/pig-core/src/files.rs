//! Cross-platform "open in system file manager": macOS Finder / Windows Explorer / Linux xdg-open.

use std::path::Path;

use crate::NoConsoleExt as _;

/// Open a directory in the system file manager. Detached spawn: does not wait for the exit
/// code (Windows explorer often returns non-zero even on success); only spawn failure is an error
pub fn open_in_file_manager(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("explorer");
        c.arg(path);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    cmd.no_console().spawn().map(|_| ())
}

/// macOS: fetch the PNG data of Finder.app's current icon for the title bar's "open in Finder"
/// button (same idea as ZCode: the platform layer fetches the real app icon instead of a
/// hand-drawn/glyph icon). NSWorkspace is thread-safe and can be called in the background; any
/// step failing returns None, and the caller falls back to a generic folder icon.
#[cfg(target_os = "macos")]
pub fn finder_icon_png() -> Option<Vec<u8>> {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{
        NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSWorkspace,
    };
    use objc2_foundation::{NSDictionary, NSString};

    let workspace = NSWorkspace::sharedWorkspace();
    let icon = workspace.iconForFile(&NSString::from_str(
        "/System/Library/CoreServices/Finder.app",
    ));
    let tiff = icon.TIFFRepresentation()?;
    // TIFF is multi-page (all icns resolutions); iterate all reps and pick the largest by pixel count to convert to PNG
    let reps = NSBitmapImageRep::imageRepsWithData(&tiff);
    let best = reps
        .iter()
        .max_by_key(|rep| rep.pixelsWide() * rep.pixelsHigh())?;
    let bitmap = best.downcast_ref::<NSBitmapImageRep>()?;
    let props: Retained<NSDictionary<NSBitmapImageRepPropertyKey, AnyObject>> = NSDictionary::new();
    // SAFETY: properties is an empty dictionary; no keys to fill, no type requirements
    let png =
        unsafe { bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }?;
    let png = png.to_vec();
    // Rendering the 1024px original directly and letting the GPU bilinearly shrink it to ~32px looks
    // blurry (no mipmaps, texel sampling); pre-scale with Lanczos3 to the target size (button size_4=16pt; 64px covers up to 4x density)
    let img = image::load_from_memory(&png).ok()?;
    let scaled = img.resize_exact(
        FM_ICON_PX,
        FM_ICON_PX,
        image::imageops::FilterType::Lanczos3,
    );
    let mut buf = std::io::Cursor::new(Vec::new());
    scaled.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

/// Output pixel edge length of the "open in Finder" button icon (size_4 = 16pt x 4x density cap)
#[cfg(target_os = "macos")]
const FM_ICON_PX: u32 = 64;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn finder_icon_png_extracts() {
        let png = finder_icon_png().expect("get Finder icon");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        // Pre-scaled to the button's target pixels (GPU shrinking the 1024px original directly looks blurry)
        let img = image::load_from_memory(&png).expect("parse PNG");
        assert_eq!((img.width(), img.height()), (FM_ICON_PX, FM_ICON_PX));
    }
}
