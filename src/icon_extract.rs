use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use crate::identity::AppIdentity;

/// Extracted icon bitmap in straight (unpremultiplied) RGBA format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconRgba {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Canonical cache key shared by running apps, favorites, and installed apps.
pub fn cache_key(identity: &AppIdentity, exe_path: Option<&str>) -> String {
    cache_key_for(identity, exe_path, None)
}

/// Cache key that additionally distinguishes a shortcut's custom icon file.
pub fn cache_key_for(
    identity: &AppIdentity,
    exe_path: Option<&str>,
    icon_override: Option<&str>,
) -> String {
    match identity {
        AppIdentity::Msix { aumid } => format!("aumid:{}", aumid.to_lowercase()),
        AppIdentity::Win32 { .. } | AppIdentity::Manual { .. } => {
            if let Some(ov) = icon_override {
                format!("icon:{}", ov.to_lowercase())
            } else if let Some(p) = exe_path {
                format!("exe:{}", p.to_lowercase())
            } else {
                format!("id:{}", identity.display_name().to_lowercase())
            }
        }
    }
}

/// Extract an icon for `identity` (MSIX via Shell image factory, Win32 via EXE).
/// Uses a two-level cache: process-wide memory cache (L1) + `%APPDATA%\ReLaunch\icon_cache`
/// disk cache (L2). Never holds the memory lock during disk or Shell extraction.
pub fn extract_app_icon(identity: &AppIdentity, exe_path: Option<&str>) -> Option<IconRgba> {
    extract_app_icon_with_override(identity, exe_path, None)
}

/// Same as [`extract_app_icon`], preferring `icon_override` (e.g. a `.lnk` custom `.ico`)
/// for Win32/Manual apps when present.
pub fn extract_app_icon_with_override(
    identity: &AppIdentity,
    exe_path: Option<&str>,
    icon_override: Option<&str>,
) -> Option<IconRgba> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<IconRgba>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = cache_key_for(identity, exe_path, icon_override);

    if let Some(cached) = cache.lock().unwrap().get(&key) {
        return cached.clone();
    }

    if let Some(disk_icon) = read_disk_cache(&key) {
        cache.lock().unwrap().insert(key, Some(disk_icon.clone()));
        return Some(disk_icon);
    }

    let icon = match identity {
        AppIdentity::Msix { aumid } => {
            extract_shell_image(&format!("shell:AppsFolder\\{aumid}"), 64)
                .or_else(|| exe_path.and_then(|p| extract_shell_image(p, 64)))
                .or_else(|| exe_path.and_then(extract_exe_hicon_fallback))
        }
        AppIdentity::Win32 { .. } | AppIdentity::Manual { .. } => icon_override
            .and_then(|ov| extract_shell_image(ov, 64).or_else(|| extract_exe_hicon_fallback(ov)))
            .or_else(|| {
                exe_path.and_then(|p| {
                    extract_shell_image(p, 64).or_else(|| extract_exe_hicon_fallback(p))
                })
            }),
    };

    if let Some(ref ic) = icon {
        write_disk_cache(&key, ic);
    }

    cache.lock().unwrap().insert(key, icon.clone());
    icon
}

// ---------------------------------------------------------------------------
// L2 disk cache (RLIC binary format)
// ---------------------------------------------------------------------------

const RLIC_MAGIC: &[u8; 4] = b"RLI2";

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn disk_cache_path(key: &str) -> Option<PathBuf> {
    let config_path = crate::config::Config::config_path();
    let dir = config_path.parent()?.join("icon_cache");
    Some(dir.join(format!("{:016x}.rlic", fnv1a_64(key.as_bytes()))))
}

fn encode_rlic(icon: &IconRgba) -> Option<Vec<u8>> {
    if !(1..=256).contains(&icon.width) || !(1..=256).contains(&icon.height) {
        return None;
    }
    let expected = (icon.width as usize) * (icon.height as usize) * 4;
    if icon.rgba.len() != expected {
        return None;
    }
    let mut out = Vec::with_capacity(8 + expected);
    out.extend_from_slice(RLIC_MAGIC);
    out.extend_from_slice(&(icon.width as u16).to_le_bytes());
    out.extend_from_slice(&(icon.height as u16).to_le_bytes());
    out.extend_from_slice(&icon.rgba);
    Some(out)
}

fn decode_rlic(bytes: &[u8]) -> Option<IconRgba> {
    if bytes.len() < 8 || &bytes[0..4] != RLIC_MAGIC {
        return None;
    }
    let width = u16::from_le_bytes([bytes[4], bytes[5]]) as u32;
    let height = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
    if !(1..=256).contains(&width) || !(1..=256).contains(&height) {
        return None;
    }
    let expected = 8 + (width as usize) * (height as usize) * 4;
    if bytes.len() != expected {
        return None;
    }
    Some(IconRgba {
        width,
        height,
        rgba: bytes[8..].to_vec(),
    })
}

fn read_disk_cache(key: &str) -> Option<IconRgba> {
    let path = disk_cache_path(key)?;
    let bytes = std::fs::read(path).ok()?;
    decode_rlic(&bytes)
}

fn write_disk_cache(key: &str, icon: &IconRgba) {
    let Some(path) = disk_cache_path(key) else {
        return;
    };
    let Some(bytes) = encode_rlic(icon) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("rlic.tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

// ---------------------------------------------------------------------------
// High-res icon extraction via IShellItemImageFactory (MSIX + Win32)
// ---------------------------------------------------------------------------

fn extract_shell_image(parsing_name: &str, size: i32) -> Option<IconRgba> {
    use windows::Win32::{
        Foundation::SIZE,
        Graphics::Gdi::DeleteObject,
        UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY},
    };
    use windows::core::PCWSTR;

    let _com = crate::com::ComGuard::init_sta();
    let wide: Vec<u16> = parsing_name.encode_utf16().chain([0]).collect();

    unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let hbm = factory
            .GetImage(SIZE { cx: size, cy: size }, SIIGBF_ICONONLY)
            .ok()?;
        let icon = hbitmap_to_rgba(hbm);
        let _ = DeleteObject(hbm);
        icon
    }
}

fn hbitmap_to_rgba(hbm: windows::Win32::Graphics::Gdi::HBITMAP) -> Option<IconRgba> {
    use windows::Win32::{
        Foundation::HWND,
        Graphics::Gdi::{
            CreateCompatibleDC, DeleteDC, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP,
            BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        },
    };

    unsafe {
        let mut bm = BITMAP::default();
        let got = GetObjectW(
            hbm,
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut BITMAP as *mut std::ffi::c_void),
        );
        if got == 0 {
            return None;
        }
        let w = bm.bmWidth;
        let h = bm.bmHeight.abs();
        if !(1..=256).contains(&w) || !(1..=256).contains(&h) {
            return None;
        }

        let screen_dc = GetDC(HWND(std::ptr::null_mut()));
        let mem_dc = CreateCompatibleDC(screen_dc);
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let rows = GetDIBits(
            mem_dc,
            hbm,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND(std::ptr::null_mut()), screen_dc);

        if rows == 0 {
            return None;
        }

        let mut rgba = bgra_to_rgba(&buf);
        fix_legacy_zero_alpha(&mut rgba);
        unpremultiply_if_needed(&mut rgba);
        Some(IconRgba {
            width: w as u32,
            height: h as u32,
            rgba,
        })
    }
}

// ---------------------------------------------------------------------------
// Win32 EXE fallback icon extraction via SHGetFileInfoW
// ---------------------------------------------------------------------------

fn extract_exe_hicon_fallback(exe_path: &str) -> Option<IconRgba> {
    use windows::Win32::{
        Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES,
        UI::{
            Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON},
            WindowsAndMessaging::DestroyIcon,
        },
    };
    use windows::core::PCWSTR;

    let wide: Vec<u16> = exe_path.encode_utf16().chain([0]).collect();
    let mut fi = SHFILEINFOW::default();

    // SHGetFileInfoW returns 0 on failure.
    let ret = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut fi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    if ret == 0 || fi.hIcon.is_invalid() {
        return None;
    }

    let rgba = hicon_to_rgba(fi.hIcon);
    unsafe {
        let _ = DestroyIcon(fi.hIcon);
    }
    rgba
}

/// Convert a Windows HICON (any size) to 32×32 RGBA bytes.
fn hicon_to_rgba(hicon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<IconRgba> {
    use windows::Win32::{
        Foundation::HWND,
        Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
            ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        },
        UI::WindowsAndMessaging::{DrawIconEx, DI_NORMAL},
    };

    const W: i32 = 32;
    const H: i32 = 32;

    unsafe {
        let screen_dc = GetDC(HWND(std::ptr::null_mut()));
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bmp = CreateCompatibleBitmap(screen_dc, W, H);
        let old = SelectObject(mem_dc, bmp);

        let _ = DrawIconEx(mem_dc, 0, 0, hicon, W, H, 0, None, DI_NORMAL);

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: W,
                biHeight: -H,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (W * H * 4) as usize];
        let rows = GetDIBits(
            mem_dc,
            bmp,
            0,
            H as u32,
            Some(buf.as_mut_ptr() as _),
            &mut bmi,
            DIB_RGB_COLORS,
        );

        SelectObject(mem_dc, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND(std::ptr::null_mut()), screen_dc);

        if rows == 0 {
            return None;
        }

        let mut rgba = bgra_to_rgba(&buf);
        fix_legacy_zero_alpha(&mut rgba);
        unpremultiply_if_needed(&mut rgba);
        Some(IconRgba {
            width: W as u32,
            height: H as u32,
            rgba,
        })
    }
}

// ---------------------------------------------------------------------------
// Pixel format helpers
// ---------------------------------------------------------------------------

fn bgra_to_rgba(buf: &[u8]) -> Vec<u8> {
    let mut rgba = vec![0u8; buf.len()];
    for i in (0..buf.len()).step_by(4) {
        rgba[i] = buf[i + 2];
        rgba[i + 1] = buf[i + 1];
        rgba[i + 2] = buf[i];
        rgba[i + 3] = buf[i + 3];
    }
    rgba
}

fn fix_legacy_zero_alpha(rgba: &mut [u8]) {
    if !rgba.chunks_exact(4).any(|px| px[3] != 0) {
        for px in rgba.chunks_exact_mut(4) {
            if px[0] != 0 || px[1] != 0 || px[2] != 0 {
                px[3] = 255;
            }
        }
    }
}

/// Convert premultiplied-alpha bitmaps back to straight alpha when detected.
fn unpremultiply_if_needed(rgba: &mut [u8]) {
    let mut has_semi = false;
    for px in rgba.chunks_exact(4) {
        let a = px[3];
        if px[0] > a || px[1] > a || px[2] > a {
            return;
        }
        if a > 0 && a < 255 {
            has_semi = true;
        }
    }
    if !has_semi {
        return;
    }
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3];
        if a > 0 && a < 255 {
            let af = a as u16;
            px[0] = (((px[0] as u16 * 255) + af / 2) / af).min(255) as u8;
            px[1] = (((px[1] as u16 * 255) + af / 2) / af).min(255) as u8;
            px[2] = (((px[2] as u16 * 255) + af / 2) / af).min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_consistency() {
        let msix = AppIdentity::Msix {
            aumid: "Claude_pzs8sxrjxfjjc!Claude".to_string(),
        };
        assert_eq!(cache_key(&msix, None), "aumid:claude_pzs8sxrjxfjjc!claude");
        assert_eq!(
            cache_key(&msix, Some(r"C:\Dummy\claude.exe")),
            "aumid:claude_pzs8sxrjxfjjc!claude"
        );

        let win32 = AppIdentity::Win32 {
            exe_name: "code.exe".to_string(),
            product_name: "Visual Studio Code".to_string(),
        };
        assert_eq!(
            cache_key(&win32, Some(r"D:\Microsoft VS Code\Code.exe")),
            r"exe:d:\microsoft vs code\code.exe"
        );
    }

    #[test]
    fn disk_format_roundtrip_and_validation() {
        let icon = IconRgba {
            width: 2,
            height: 2,
            rgba: vec![
                1, 2, 3, 255, 4, 5, 6, 128, 7, 8, 9, 0, 10, 11, 12, 255,
            ],
        };
        let encoded = encode_rlic(&icon).expect("valid icon should encode");
        assert_eq!(decode_rlic(&encoded), Some(icon));

        assert_eq!(decode_rlic(b"RLIC\x01\x00\x01\x00\0\0\0\0"), None);
        assert_eq!(decode_rlic(b"BAD!1234"), None);
        assert_eq!(decode_rlic(&encoded[..encoded.len() - 1]), None);
    }

    #[test]
    fn unpremultiply_behavior() {
        let mut premul = vec![64, 128, 0, 128, 255, 255, 255, 255];
        unpremultiply_if_needed(&mut premul);
        assert_eq!(premul, vec![128, 255, 0, 128, 255, 255, 255, 255]);

        let mut straight = vec![200, 100, 50, 128];
        unpremultiply_if_needed(&mut straight);
        assert_eq!(straight, vec![200, 100, 50, 128]);
    }
}
