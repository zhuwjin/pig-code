//! 跨平台「在系统文件管理器中打开」：macOS 访达 / Windows 资源管理器 / Linux xdg-open。

use std::path::Path;

use crate::NoConsoleExt as _;

/// 文件管理器的平台称呼（菜单标签、tooltip 等 UI 文案用）
pub fn file_manager_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "访达"
    }
    #[cfg(target_os = "windows")]
    {
        "文件资源管理器"
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "文件管理器"
    }
}

/// 在系统文件管理器中打开目录。detached spawn：不等退出码
///（Windows explorer 成功也常返回非零码），只有启动失败才报错
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

/// macOS：取访达（Finder.app）当前图标的 PNG 数据，给标题栏「在访达中打开」
/// 按钮用（ZCode 同思路：平台层取真实 App 图标，而非自绘/字形图标）。
/// NSWorkspace 线程安全可后台调；任一步失败返回 None，调用方回退通用文件夹图标。
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
    // TIFF 是多页（icns 全部分辨率），遍历全部 rep 挑像素最大的转 PNG
    let reps = NSBitmapImageRep::imageRepsWithData(&tiff);
    let best = reps
        .iter()
        .max_by_key(|rep| rep.pixelsWide() * rep.pixelsHigh())?;
    let bitmap = best.downcast_ref::<NSBitmapImageRep>()?;
    let props: Retained<NSDictionary<NSBitmapImageRepPropertyKey, AnyObject>> = NSDictionary::new();
    // SAFETY: properties 传空字典，无可填键，无类型要求
    let png =
        unsafe { bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }?;
    let png = png.to_vec();
    // 1024px 原图直接上屏，GPU 双线性缩到 ~32px（无 mipmap 抽 texel 发糊）；
    // 按目标尺寸 Lanczos3 预缩放（按钮 size_4=16pt，64px 覆盖到 4x 密度）
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

/// 「在访达中打开」按钮图标的输出像素边长（size_4 = 16pt × 4x 密度上限）
#[cfg(target_os = "macos")]
const FM_ICON_PX: u32 = 64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_manager_name_non_empty() {
        assert!(!file_manager_name().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn finder_icon_png_extracts() {
        let png = finder_icon_png().expect("取访达图标");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        // 预缩放到按钮目标像素（GPU 直缩 1024px 原图发糊）
        let img = image::load_from_memory(&png).expect("解析 PNG");
        assert_eq!((img.width(), img.height()), (FM_ICON_PX, FM_ICON_PX));
    }
}
