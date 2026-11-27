#[path = "src/icon.rs"]
mod icon;

use std::path::PathBuf;

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}" />
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="urn:schemas-microsoft-com:SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
    </windowsSettings>
  </application>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"
      />
    </dependentAssembly>
  </dependency>
</assembly>
"#;

fn encode_dib_frame(size: u32, rgba: &[u8]) -> Vec<u8> {
    let w = size as usize;
    let h = size as usize;
    let xor_len = w * h * 4;
    let mask_row_bytes = ((w + 31) / 32) * 4;
    let and_len = mask_row_bytes * h;
    let total = 40 + xor_len + and_len;
    let mut out = Vec::with_capacity(total);

    // BITMAPINFOHEADER (40 bytes)
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(size as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // biHeight (XOR + AND)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression (BI_RGB)
    out.extend_from_slice(&((xor_len + and_len) as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // Bottom-up BGRA XOR pixels.
    for y in (0..h).rev() {
        for x in 0..w {
            let idx = (y * w + x) * 4;
            out.push(rgba[idx + 2]); // B
            out.push(rgba[idx + 1]); // G
            out.push(rgba[idx]);     // R
            out.push(rgba[idx + 3]); // A
        }
    }

    // Bottom-up 1bpp AND mask (bit = 1 where pixel alpha == 0).
    for y in (0..h).rev() {
        let mut row = vec![0u8; mask_row_bytes];
        for x in 0..w {
            let alpha = rgba[(y * w + x) * 4 + 3];
            if alpha == 0 {
                row[x / 8] |= 1 << (7 - (x % 8));
            }
        }
        out.extend_from_slice(&row);
    }

    out
}

fn encode_png_frame(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    let encoder = PngEncoder::new(&mut buf);
    encoder
        .write_image(rgba, size, size, ExtendedColorType::Rgba8)
        .expect("failed to encode PNG icon frame");
    buf
}

fn build_multi_ico() -> Vec<u8> {
    let sizes: &[u32] = &[16, 32, 48, 64, 256];
    let frames: Vec<(u32, Vec<u8>)> = sizes
        .iter()
        .map(|&size| {
            let rgba = icon::rgba(size);
            let data = if size >= 256 {
                encode_png_frame(size, &rgba)
            } else {
                encode_dib_frame(size, &rgba)
            };
            (size, data)
        })
        .collect();

    let count = frames.len() as u16;
    let header_size = 6 + frames.len() * 16;
    let payload_size: usize = frames.iter().map(|(_, d)| d.len()).sum();
    let mut ico = Vec::with_capacity(header_size + payload_size);

    // ICONDIR (6 bytes)
    ico.extend_from_slice(&0u16.to_le_bytes()); // idReserved
    ico.extend_from_slice(&1u16.to_le_bytes()); // idType (1 = icon)
    ico.extend_from_slice(&count.to_le_bytes()); // idCount

    // ICONDIRENTRY (16 bytes each)
    let mut offset = header_size as u32;
    for (size, data) in &frames {
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        ico.push(dim); // bWidth
        ico.push(dim); // bHeight
        ico.push(0); // bColorCount
        ico.push(0); // bReserved
        ico.extend_from_slice(&1u16.to_le_bytes()); // wPlanes
        ico.extend_from_slice(&32u16.to_le_bytes()); // wBitCount
        ico.extend_from_slice(&(data.len() as u32).to_le_bytes()); // dwBytesInRes
        ico.extend_from_slice(&offset.to_le_bytes()); // dwImageOffset
        offset += data.len() as u32;
    }

    for (_, data) in &frames {
        ico.extend_from_slice(data);
    }

    ico
}

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let ico_bytes = build_multi_ico();

    let out_ico = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR missing")).join("relaunch.ico");
    std::fs::write(&out_ico, &ico_bytes).expect("failed to write OUT_DIR/relaunch.ico");

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR missing"));
    let assets_dir = manifest_dir.join("assets");
    let _ = std::fs::create_dir_all(&assets_dir);
    let asset_ico = assets_dir.join("relaunch.ico");
    let needs_write = match std::fs::read(&asset_ico) {
        Ok(existing) => existing != ico_bytes,
        Err(_) => true,
    };
    if needs_write {
        std::fs::write(&asset_ico, &ico_bytes).expect("failed to write assets/relaunch.ico");
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon(out_ico.to_str().expect("non-UTF-8 OUT_DIR path"));
    res.set("ProductName", "ReLaunch");
    res.set("FileDescription", "Restart any app in one click");
    res.set("InternalName", "ReLaunch");
    res.set("OriginalFilename", "ReLaunch.exe");
    res.set_manifest(MANIFEST);
    res.compile().expect("failed to compile Windows resources");
}
