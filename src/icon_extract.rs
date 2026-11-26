/// Extract the 32×32 RGBA icon for an application from its EXE path.
/// Returns None when the shell cannot provide an icon.
/// Must be called on a thread that has COM initialised (the discovery
/// thread already does this; the UI thread does not need it for icons).
pub fn extract_exe_icon(exe_path: &str) -> Option<Vec<u8>> {
    use windows::Win32::{
        Foundation::HINSTANCE,
        Graphics::Gdi::{
            CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, SelectObject,
            BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        },
        UI::{
            Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_FLAGS, SHGFI_ICON, SHGFI_LARGEICON},
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
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut fi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    if ret == 0 || fi.hIcon.is_invalid() {
        return None;
    }

    let rgba = hicon_to_rgba(fi.hIcon);
    unsafe { let _ = DestroyIcon(fi.hIcon); }
    rgba
}

/// Convert a Windows HICON (any size) to 32×32 RGBA bytes.
fn hicon_to_rgba(hicon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<Vec<u8>> {
    use windows::Win32::{
        Foundation::HWND,
        Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
            GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
            DIB_RGB_COLORS,
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

        // Fill with transparent black first.
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: W,
                biHeight: -H, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (W * H * 4) as usize];
        GetDIBits(mem_dc, bmp, 0, H as u32, Some(buf.as_mut_ptr() as _), &mut bmi, DIB_RGB_COLORS);

        // Draw icon onto the bitmap.
        let _ = DrawIconEx(mem_dc, 0, 0, hicon, W, H, 0, None, DI_NORMAL);

        // Read back pixels (BGRx).
        let rows = GetDIBits(mem_dc, bmp, 0, H as u32, Some(buf.as_mut_ptr() as _), &mut bmi, DIB_RGB_COLORS);

        SelectObject(mem_dc, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND(std::ptr::null_mut()), screen_dc);

        if rows == 0 {
            return None;
        }

        // Convert BGRx → RGBA.
        let mut rgba = vec![0u8; buf.len()];
        for i in (0..buf.len()).step_by(4) {
            rgba[i]     = buf[i + 2]; // R
            rgba[i + 1] = buf[i + 1]; // G
            rgba[i + 2] = buf[i];     // B
            rgba[i + 3] = buf[i + 3]; // A
        }
        Some(rgba)
    }
}
