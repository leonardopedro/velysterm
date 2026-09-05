//! Minimal PDF writer for the paginated raster export (`--pages-pdf`).
//!
//! Typst has no native PDF *export* in the version this project pins
//! (only a PDF *image loader*), so the pages are produced by Typst's
//! own page model and rasterized through typst_imaging
//! ([`crate::export::doc_pages_image`]); this module only *wraps*
//! the resulting page bitmaps in a minimal PDF container — one
//! page object per image, each a FlateDecode-compressed DeviceRGB
//! bitmap (alpha composited over white). There is no text or vector
//! layer: the PDF is the rasterized document, exactly what the
//! whole-graphics pipeline produces.

use imaging::RgbaImage;

/// Flate-compress a page raster as DeviceRGB scanlines (PDF
/// `FlateDecode` is zlib-wrapped deflate). Alpha is composited over
/// white so the bitmap needs no alpha channel.
fn rgb_flate(img: &RgbaImage) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let mut rgb = Vec::with_capacity((img.width * img.height * 3) as usize);
    for px in img.data.chunks_exact(4) {
        let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        // out = src*a + white*(255-a), /255.
        rgb.push(((r * a + 255 * (255 - a)) / 255) as u8);
        rgb.push(((g * a + 255 * (255 - a)) / 255) as u8);
        rgb.push(((b * a + 255 * (255 - a)) / 255) as u8);
    }
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&rgb)
        .map_err(|e| format!("flate encode failed: {e}"))?;
    enc.finish()
        .map_err(|e| format!("flate encode failed: {e}"))
}

/// Wrap paginated page rasters into a minimal PDF document.
///
/// Object numbering: 1 = Catalog, 2 = Pages, then per page *i*:
/// 3+3i = Page, 4+3i = Contents (a single `cm`/`Do` that scales the
/// image to the page's MediaBox, which is set to the raster's pixel
/// size — the pages are 1 px/pt as rasterized), 5+3i = Image XObject.
pub fn pages_pdf(pages: &[RgbaImage]) -> Result<Vec<u8>, String> {
    let n = pages.len();
    if n == 0 {
        return Err("no pages to write".into());
    }
    // Object bodies, built before serialization so the xref offsets
    // are exact.
    let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(2 + 3 * n);
    bodies.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    let kids: String = (0..n).map(|i| format!("{} 0 R ", 3 + 3 * i)).collect();
    bodies.push(format!("<< /Type /Pages /Kids [{}]/Count {n} >>", kids.trim()).into_bytes());
    for (i, img) in pages.iter().enumerate() {
        let content_obj = 4 + 3 * i;
        let image_obj = 5 + 3 * i;
        // Page object.
        bodies.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] \
                 /Contents {content_obj} 0 R \
                 /Resources << /XObject << /Im0 {image_obj} 0 R >> >> >>",
                img.width, img.height
            )
            .into_bytes(),
        );
        // Contents: paint the image across the whole media box.
        let content = format!("q {} 0 0 {} 0 0 cm /Im0 Do Q", img.width, img.height);
        let mut cstream = Vec::new();
        cstream.extend_from_slice(b"<< /Length ");
        cstream.extend_from_slice(content.len().to_string().as_bytes());
        cstream.extend_from_slice(b" >>\nstream\n");
        cstream.extend_from_slice(content.as_bytes());
        cstream.extend_from_slice(b"\nendstream");
        bodies.push(cstream);
        // Image XObject (FlateDecode DeviceRGB).
        let compressed = rgb_flate(img)?;
        let mut istream = Vec::new();
        istream.extend_from_slice(
            format!(
                "<< /Type /XObject /Subtype /Image /Width {} /Height {} \
                 /ColorSpace /DeviceRGB /BitsPerComponent 8 \
                 /Filter /FlateDecode /Length {} >>\nstream\n",
                img.width,
                img.height,
                compressed.len()
            )
            .as_bytes(),
        );
        istream.extend_from_slice(&compressed);
        istream.extend_from_slice(b"\nendstream");
        bodies.push(istream);
    }
    // Serialize with a classic xref table.
    let mut out = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n");
    let mut offsets: Vec<usize> = Vec::with_capacity(bodies.len());
    for (idx, body) in bodies.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    let size = bodies.len() + 1;
    out.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n")
            .as_bytes(),
    );
    Ok(out)
}
