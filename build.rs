//! 构建脚本：把 `assets/app.svg` 渲染成多尺寸 .ico，然后通过 Win32
//! 资源段链接进 exe。
//!
//! 流程：
//!   1. 用 resvg/usvg 解析 SVG 并光栅化到 256/48/32/16 四个尺寸 RGBA buffer
//!   2. 用 image crate 编码每个尺寸为 PNG（ICO 7+ 支持内嵌 PNG 而不是 BMP，
//!      文件更小，Win11 也支持完美）
//!   3. 手写 ICO 文件头 + 目录条目，把所有 PNG 顺序写进
//!   4. 调 embed_resource::compile 生成 .res 并加入链接器命令
//!
//! 失败处理：所有错误都 panic 让 cargo 报告，因为图标缺失意味着发布版本
//! 会显示默认 Windows 图标，是严重的视觉退化。
//! 但 SVG 文件本身缺失则 skip（允许开发期无图标也能构建）。

use std::fs;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=assets/app.svg");
    println!("cargo:rerun-if-changed=app.rc");

    // 仅 Windows 目标才需要图标资源
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }

    let svg_path = PathBuf::from("assets/app.svg");
    if !svg_path.exists() {
        // 开发期可能还没 svg，跳过图标但不阻断构建
        println!("cargo:warning=assets/app.svg not found, skipping icon embed");
        return;
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico_path = out_dir.join("app.ico");

    // 渲染 SVG → ICO
    if let Err(e) = render_svg_to_ico(&svg_path, &ico_path) {
        panic!("Failed to build app.ico from SVG: {}", e);
    }

    // 生成 app.rc 指向我们的 ico（路径用绝对路径以免 rc.exe 找不到）
    let ico_path_str = ico_path.display().to_string().replace('\\', "/");
    let rc_content = format!("1 ICON \"{}\"\n", ico_path_str);
    let rc_path = out_dir.join("app.rc");
    fs::write(&rc_path, rc_content).expect("write app.rc");

    // 编译并链接 .rc → .res → exe 资源段
    embed_resource::compile(&rc_path, embed_resource::NONE);
}

fn render_svg_to_ico(svg_path: &PathBuf, ico_path: &PathBuf) -> Result<(), String> {
    let svg_data = fs::read(svg_path).map_err(|e| format!("read svg: {}", e))?;
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(&svg_data, &opt)
        .map_err(|e| format!("parse svg: {}", e))?;

    // ICO 标准支持的尺寸 —— 256 给 alt-tab，48 给桌面，32 给标题栏，16 给托盘
    let sizes: [u32; 4] = [256, 48, 32, 16];
    let mut pngs: Vec<(u32, Vec<u8>)> = Vec::with_capacity(sizes.len());

    for &size in &sizes {
        let mut pixmap = tiny_skia::Pixmap::new(size, size)
            .ok_or_else(|| format!("alloc pixmap {}x{}", size, size))?;
        // SVG 是 256x256 viewBox；resvg 需要把它缩放到目标 size
        let scale = size as f32 / 256.0;
        let transform = tiny_skia::Transform::from_scale(scale, scale);
        resvg::render(&tree, transform, &mut pixmap.as_mut());

        // tiny-skia 输出 RGBA premultiplied；写 PNG 前要 unpremultiply 否则
        // 颜色会偏暗。resvg 默认输出 premultiplied，PNG 期望 straight alpha。
        let raw = pixmap.data();
        let mut straight: Vec<u8> = Vec::with_capacity(raw.len());
        for chunk in raw.chunks_exact(4) {
            let (r, g, b, a) = (chunk[0], chunk[1], chunk[2], chunk[3]);
            if a == 0 {
                straight.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                let inv = 255.0 / a as f32;
                straight.push((r as f32 * inv).min(255.0) as u8);
                straight.push((g as f32 * inv).min(255.0) as u8);
                straight.push((b as f32 * inv).min(255.0) as u8);
                straight.push(a);
            }
        }

        let png = encode_png_rgba(&straight, size, size)?;
        pngs.push((size, png));
    }

    write_ico(ico_path, &pngs).map_err(|e| format!("write ico: {}", e))?;
    Ok(())
}

/// 把 RGBA buffer 编码成最小 PNG（无外部依赖：单 IDAT chunk，filter type 0）。
/// 我们不用 zlib best 压缩 —— 用 store-only deflate（type 0 block）保证零
/// 依赖且足够小（256x256 ico ~256KB，可接受）。
fn encode_png_rgba(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    use std::io::Cursor;
    let mut out = Cursor::new(Vec::new());
    // PNG 签名
    out.write_all(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]).unwrap();

    // IHDR
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8);  // bit depth
    ihdr.push(6);  // color type = RGBA
    ihdr.push(0);  // compression
    ihdr.push(0);  // filter
    ihdr.push(0);  // interlace
    write_chunk(&mut out, b"IHDR", &ihdr);

    // IDAT —— 每行前缀一个 filter byte (0 = None)
    let mut raw = Vec::with_capacity((height * (1 + width * 4)) as usize);
    let stride = (width * 4) as usize;
    for y in 0..height as usize {
        raw.push(0); // filter: None
        raw.extend_from_slice(&rgba[y * stride..(y + 1) * stride]);
    }
    let compressed = deflate_store(&raw);
    write_chunk(&mut out, b"IDAT", &compressed);

    // IEND
    write_chunk(&mut out, b"IEND", &[]);

    Ok(out.into_inner())
}

fn write_chunk<W: Write>(w: &mut W, type_: &[u8; 4], data: &[u8]) {
    let len = data.len() as u32;
    w.write_all(&len.to_be_bytes()).unwrap();
    w.write_all(type_).unwrap();
    w.write_all(data).unwrap();
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(type_);
    crc_input.extend_from_slice(data);
    let crc = crc32(&crc_input);
    w.write_all(&crc.to_be_bytes()).unwrap();
}

/// zlib stream（deflate type-0 store-only blocks）+ Adler32 校验。
/// 不压缩但格式合法，足够 PNG decoder 接受。
fn deflate_store(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 64);
    // zlib header: CM=8, CINFO=7, FCHECK 凑数
    out.push(0x78);
    out.push(0x01);

    const MAX_BLOCK: usize = 65535;
    let mut pos = 0;
    while pos < data.len() {
        let chunk_len = (data.len() - pos).min(MAX_BLOCK);
        let is_final = pos + chunk_len == data.len();
        out.push(if is_final { 0x01 } else { 0x00 }); // BFINAL + BTYPE=00
        let len = chunk_len as u16;
        out.extend_from_slice(&len.to_le_bytes());
        let nlen = !len;
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(&data[pos..pos + chunk_len]);
        pos += chunk_len;
    }
    let adler = adler32(data);
    out.extend_from_slice(&adler.to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    // PNG/ZIP 用的标准 CRC-32 (poly 0xEDB88320)，单查表实现
    static mut TABLE: [u32; 256] = [0; 256];
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| unsafe {
        for n in 0..256u32 {
            let mut c = n;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
            }
            TABLE[n as usize] = c;
        }
    });
    let mut c = 0xFFFFFFFFu32;
    unsafe {
        for &b in data {
            c = TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
    }
    c ^ 0xFFFFFFFF
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &v in data {
        a = (a + v as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// 写 .ico 二进制：6B ICONDIR + N×16B 目录条目 + N 个 PNG payload 依次跟在
/// 末尾。每个目录条目记录该图像的 offset+size，使读者能定位。
fn write_ico(path: &PathBuf, pngs: &[(u32, Vec<u8>)]) -> std::io::Result<()> {
    let mut out: Vec<u8> = Vec::new();

    // ICONDIR
    out.extend_from_slice(&[0, 0]);          // reserved
    out.extend_from_slice(&[1, 0]);          // type = ICO
    out.extend_from_slice(&(pngs.len() as u16).to_le_bytes()); // count

    // 先占位目录条目，之后 patch offset
    let dir_start = out.len();
    for _ in pngs {
        out.extend_from_slice(&[0u8; 16]);
    }

    let mut payload_offsets: Vec<u32> = Vec::with_capacity(pngs.len());
    for (_size, png) in pngs {
        payload_offsets.push(out.len() as u32);
        out.extend_from_slice(png);
    }

    // 回填 16B 目录条目
    for (i, (size, png)) in pngs.iter().enumerate() {
        let entry = dir_start + i * 16;
        let sz_byte = if *size >= 256 { 0u8 } else { *size as u8 };
        out[entry] = sz_byte;            // width
        out[entry + 1] = sz_byte;        // height
        out[entry + 2] = 0;              // color count (0 = >256)
        out[entry + 3] = 0;              // reserved
        out[entry + 4..entry + 6].copy_from_slice(&1u16.to_le_bytes()); // planes
        out[entry + 6..entry + 8].copy_from_slice(&32u16.to_le_bytes()); // bpp
        out[entry + 8..entry + 12].copy_from_slice(&(png.len() as u32).to_le_bytes());
        out[entry + 12..entry + 16].copy_from_slice(&payload_offsets[i].to_le_bytes());
    }

    fs::write(path, out)
}
