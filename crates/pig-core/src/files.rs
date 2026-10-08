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

/// Open a directory in a terminal window. No platform exposes a reliable
/// "default terminal" query (Windows' setting is opaque console-delegation
/// GUIDs; macOS has no such system setting; Linux only has the
/// x-terminal-emulator alternatives mechanism), so each platform uses a
/// hardcoded preference with fallbacks — the same approach VS Code's external
/// terminal takes:
/// - Windows: `wt.exe -d <path>` (Windows Terminal, the Win11 default host);
///   when wt is not installed, `cmd.exe` started in the directory
/// - macOS: `open -a Terminal <path>` (Terminal.app is the de-facto default)
/// - Linux: x-terminal-emulator (respects the system alternative) then common
///   terminal names, spawned with the directory as cwd (all major terminals
///   open in the inherited working directory when no explicit flag is passed)
///
/// Detached spawn; only spawn failure is an error.
pub fn open_in_terminal(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("open");
        cmd.arg("-a").arg("Terminal").arg(path);
        cmd.no_console().spawn().map(|_| ())
    }
    #[cfg(target_os = "windows")]
    {
        let mut wt = std::process::Command::new("wt");
        wt.arg("-d").arg(path);
        if wt.no_console().spawn().is_ok() {
            Ok(())
        } else {
            let mut cmd = std::process::Command::new("cmd");
            cmd.current_dir(path);
            cmd.no_console().spawn().map(|_| ())
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for terminal in [
            "x-terminal-emulator",
            "gnome-terminal",
            "konsole",
            "kgx",
            "xfce4-terminal",
            "xterm",
        ] {
            let mut cmd = std::process::Command::new(terminal);
            cmd.current_dir(path);
            if cmd.no_console().spawn().is_ok() {
                return Ok(());
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no known terminal emulator found",
        ))
    }
}

/// macOS: fetch the PNG data of an app's current icon for the title bar's
/// "open in …" buttons (same idea as ZCode: the platform layer fetches the
/// real app icon instead of a hand-drawn/glyph icon). NSWorkspace is
/// thread-safe and can be called in the background; any step failing returns
/// None, and the caller falls back to a generic icon.
#[cfg(target_os = "macos")]
pub fn finder_icon_png() -> Option<Vec<u8>> {
    app_icon_png("/System/Library/CoreServices/Finder.app")
}

/// macOS: Terminal.app's icon for the "open in Terminal" menu row.
#[cfg(target_os = "macos")]
pub fn terminal_icon_png() -> Option<Vec<u8>> {
    app_icon_png("/System/Applications/Utilities/Terminal.app")
}

#[cfg(target_os = "macos")]
fn app_icon_png(app_path: &str) -> Option<Vec<u8>> {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{
        NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSWorkspace,
    };
    use objc2_foundation::{NSDictionary, NSString};

    let workspace = NSWorkspace::sharedWorkspace();
    let icon = workspace.iconForFile(&NSString::from_str(app_path));
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

/// Output pixel edge length of the "open in file manager" button icon (size_4 = 16pt x 4x density cap)
const FM_ICON_PX: u32 = 64;

/// Windows: fetch the PNG data of File Explorer's own application icon for the
/// title-bar "open in File Explorer" button — the icon of the app the button
/// launches, mirroring both the macOS Finder-icon path above and ZCode's
/// "open in editor" button (which shows the target editor's real app icon).
#[cfg(target_os = "windows")]
pub fn file_manager_icon_png() -> Option<Vec<u8>> {
    use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;

    // SAFETY: plain Win32 calls with no shared state (see extract_app_icon_png).
    unsafe {
        // %WINDIR%\explorer.exe in a fixed [u16; 260] buffer (the API's
        // required path shape in this windows crate version)
        let mut path = [0u16; 260];
        let dir_len = GetWindowsDirectoryW(Some(&mut path)) as usize;
        let suffix: &[u16] = &[
            92, 101, 120, 112, 108, 111, 114, 101, 114, 46, 101, 120, 101,
        ]; // "\explorer.exe"
        if dir_len == 0 || dir_len + suffix.len() >= path.len() {
            return None;
        }
        path[dir_len..dir_len + suffix.len()].copy_from_slice(suffix);
        extract_app_icon_png(&path)
    }
}

/// Windows: Windows Terminal's application icon for the "open in terminal"
/// menu row. The wt.exe app-execution alias under WindowsApps is a reparse
/// point PrivateExtractIconsW cannot read, so the packaged app's real exe is
/// resolved from the AppModel repository registry (PackageRootFolder of the
/// newest Microsoft.WindowsTerminal_* package; the Preview package has a
/// different prefix and never matches); the alias path stays as a fallback
/// for unpackaged wt.exe copies. Returns None when Windows Terminal is not
/// installed (the caller falls back to a generic terminal glyph icon).
#[cfg(target_os = "windows")]
pub fn terminal_icon_png() -> Option<Vec<u8>> {
    if let Some(exe) = windows_terminal_exe_from_registry()
        && std::fs::metadata(&exe).is_ok()
        && let Some(path) = wide_path(&exe)
        // SAFETY: see extract_app_icon_png.
        && let Some(png) = unsafe { extract_app_icon_png(&path) }
    {
        return Some(png);
    }
    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    let wt = std::path::PathBuf::from(local_app_data).join(r"Microsoft\WindowsApps\wt.exe");
    let wt = wt.to_string_lossy().into_owned();
    // SAFETY: see extract_app_icon_png.
    unsafe { extract_app_icon_png(&wide_path(&wt)?) }
}

/// Newest Microsoft.WindowsTerminal_* package root from the per-user AppModel
/// repository registry → "<root>\WindowsTerminal.exe"
#[cfg(target_os = "windows")]
fn windows_terminal_exe_from_registry() -> Option<String> {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE, REG_SAM_FLAGS,
        RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
    };
    use windows::core::{PCWSTR, PWSTR};

    const PACKAGES: &str = concat!(
        "Software\\Classes\\Local Settings\\Software\\Microsoft\\",
        "Windows\\CurrentVersion\\AppModel\\Repository\\Packages"
    );
    let to_wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };

    // SAFETY: only this thread's KEY_READ handles are touched; every handle is
    // closed on every path below.
    unsafe {
        let subkey = to_wide(PACKAGES);
        let mut packages = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            0,
            REG_SAM_FLAGS(KEY_QUERY_VALUE.0 | KEY_ENUMERATE_SUB_KEYS.0),
            &mut packages,
        )
        .is_err()
        {
            return None;
        }

        // Version-part ordering is lexicographic (good enough: machines carry
        // one version; the packaged family prefix never matches Preview)
        let mut newest: Option<String> = None;
        for index in 0.. {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(
                packages,
                index,
                PWSTR(name.as_mut_ptr()),
                &mut len,
                None,
                PWSTR::null(),
                None,
                None,
            )
            .is_err()
            {
                break;
            }
            let key = String::from_utf16_lossy(&name[..len as usize]);
            if key.starts_with("Microsoft.WindowsTerminal_")
                && newest.as_deref().is_none_or(|best| key.as_str() > best)
            {
                newest = Some(key);
            }
        }
        let newest = newest?;

        let mut root = HKEY::default();
        let opened = RegOpenKeyExW(
            packages,
            PCWSTR(to_wide(&newest).as_ptr()),
            0,
            REG_SAM_FLAGS(KEY_QUERY_VALUE.0),
            &mut root,
        )
        .is_ok();
        let _ = RegCloseKey(packages);
        if !opened {
            return None;
        }

        let value = to_wide("PackageRootFolder");
        let mut buf = [0u16; 260];
        let mut buf_len = (buf.len() * 2) as u32;
        let read = RegGetValueW(
            root,
            None,
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut buf_len),
        )
        .is_ok();
        let _ = RegCloseKey(root);
        if !read {
            return None;
        }
        let root = String::from_utf16_lossy(&buf[..(buf_len / 2) as usize]);
        let root = root.trim_end_matches('\0');
        Some(format!("{root}\\WindowsTerminal.exe"))
    }
}

/// Nul-padded fixed path buffer for the icon-extraction API
#[cfg(target_os = "windows")]
fn wide_path(path: &str) -> Option<[u16; 260]> {
    let mut wide = [0u16; 260];
    let chars: Vec<u16> = path.encode_utf16().collect();
    if chars.len() >= wide.len() {
        return None;
    }
    wide[..chars.len()].copy_from_slice(&chars);
    Some(wide)
}

/// Extract an exe's main icon (index 0) as a 64px PNG. Icon index 0 = the
/// application's main icon; requesting the target size lets the API pick/scale
/// from the best source resource. Thread-safe (GDI calls with a private DC);
/// every handle is released.
#[cfg(target_os = "windows")]
unsafe fn extract_app_icon_png(path: &[u16; 260]) -> Option<Vec<u8>> {
    use windows::Win32::Graphics::Gdi::DeleteObject;
    use windows::Win32::UI::WindowsAndMessaging::{
        DestroyIcon, GetIconInfo, ICONINFO, PrivateExtractIconsW,
    };

    // SAFETY: plain Win32 calls with no shared state; every handle is released below.
    unsafe {
        let (mut icons, mut icon_id) = ([Default::default()], 0u32);
        let extracted = PrivateExtractIconsW(
            path,
            0,
            FM_ICON_PX as i32,
            FM_ICON_PX as i32,
            Some(&mut icons),
            Some(&mut icon_id as *mut u32),
            0,
        );
        let hicon = icons[0];
        if extracted == 0 || hicon.is_invalid() {
            return None;
        }

        let icon = {
            let mut icon_info = ICONINFO::default();
            if GetIconInfo(hicon, &mut icon_info).is_err() {
                let _ = DestroyIcon(hicon);
                return None;
            }
            icon_info
        };

        let png = hbitmap_to_png(icon.hbmColor);
        if !icon.hbmMask.is_invalid() {
            let _ = DeleteObject(icon.hbmMask);
        }
        if !icon.hbmColor.is_invalid() {
            let _ = DeleteObject(icon.hbmColor);
        }
        let _ = DestroyIcon(hicon);
        png
    }
}

/// HBITMAP (32bpp icon color bitmap) → PNG bytes. Fails on monochrome (hbmColor null) or
/// non-32bpp bitmaps — the shell folder icon is always 32bpp ARGB.
#[cfg(target_os = "windows")]
unsafe fn hbitmap_to_png(bitmap: windows::Win32::Graphics::Gdi::HBITMAP) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GetDC, GetDIBits, GetObjectW,
        ReleaseDC,
    };

    // SAFETY: a private screen DC only backs the GetDIBits call and is released right after.
    unsafe {
        if bitmap.is_invalid() {
            return None;
        }
        let mut bmp = BITMAP::default();
        if GetObjectW(
            windows::Win32::Graphics::Gdi::HGDIOBJ(bitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bmp as *mut BITMAP).cast()),
        ) == 0
        {
            return None;
        }
        let (width, height) = (bmp.bmWidth as usize, bmp.bmHeight.unsigned_abs() as usize);
        if width == 0 || height == 0 || bmp.bmBitsPixel != 32 {
            return None;
        }

        let hdc = GetDC(HWND::default());
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // Negative height = top-down rows (PNG order)
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels: Vec<u8> = vec![0; width * height * 4];
        let lines = GetDIBits(
            hdc,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        ReleaseDC(HWND::default(), hdc);
        if lines == 0 {
            return None;
        }

        // BGRA → RGBA
        let (chunks, _) = pixels.as_chunks_mut::<4>();
        for chunk in chunks {
            chunk.swap(0, 2);
        }
        let out = image::RgbaImage::from_raw(width as u32, height as u32, pixels)?;
        // Downscale to the button's target size (same Lanczos3 treatment as macOS)
        let scaled = image::DynamicImage::ImageRgba8(out).resize_exact(
            FM_ICON_PX,
            FM_ICON_PX,
            image::imageops::FilterType::Lanczos3,
        );
        let mut buf = std::io::Cursor::new(Vec::new());
        scaled.write_to(&mut buf, image::ImageFormat::Png).ok()?;
        Some(buf.into_inner())
    }
}

#[cfg(test)]
mod tests {
    // The tests below are platform-specific; the imports would be unused elsewhere
    #[cfg(any(target_os = "macos", target_os = "windows"))]
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

    #[cfg(target_os = "windows")]
    #[test]
    fn file_manager_icon_png_extracts() {
        let png = file_manager_icon_png().expect("get the system folder icon");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let img = image::load_from_memory(&png).expect("parse PNG");
        assert_eq!((img.width(), img.height()), (FM_ICON_PX, FM_ICON_PX));
        // The Win11 folder icon carries color; an all-mono dump means the
        // extraction degenerated
        let rgba = img.to_rgba8();
        let colored = rgba
            .pixels()
            .any(|p| p[3] > 0 && (p[0] != p[1] || p[1] != p[2]));
        assert!(colored, "system folder icon should be colored");
    }
}
