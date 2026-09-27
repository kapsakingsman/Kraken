//! Test PDFs generated on the fly, so the repository does not need large binary files.
//!
//! Each kind stresses a different part of rendering:
//! - `text`: many pages of small text (font rasterization, the common case),
//! - `vector`: pages with tens of thousands of path segments (CAD drawings, maps),
//! - `images`: pages covered by a large uncompressed image (scans, photos).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub struct Fixtures {
    pub text: PathBuf,
    pub vector: PathBuf,
    pub images: PathBuf,
}

/// Page count of the text fixture: the "500-page PDF" from the roadmap.
pub const TEXT_PAGES: usize = 500;
pub const VECTOR_PAGES: usize = 10;
pub const IMAGE_PAGES: usize = 5;

/// Writes the fixtures into `dir` (skipping ones that already exist).
pub fn ensure(dir: &Path) -> std::io::Result<Fixtures> {
    std::fs::create_dir_all(dir)?;
    let fixtures = Fixtures {
        text: dir.join("text-500-pages.pdf"),
        vector: dir.join("vector-heavy.pdf"),
        images: dir.join("image-heavy.pdf"),
    };
    let mut rng = Rng(0x5eed);
    if !fixtures.text.exists() {
        let pages = (1..=TEXT_PAGES).map(|n| text_page(n, &mut rng)).collect();
        write_pdf(&fixtures.text, pages, None)?;
    }
    if !fixtures.vector.exists() {
        let pages = (0..VECTOR_PAGES).map(|_| vector_page(&mut rng)).collect();
        write_pdf(&fixtures.vector, pages, None)?;
    }
    if !fixtures.images.exists() {
        let pages = (0..IMAGE_PAGES)
            .map(|_| b"q 595 0 0 842 0 0 cm /Im1 Do Q".to_vec())
            .collect();
        write_pdf(&fixtures.images, pages, Some(image(1200, 1700)))?;
    }
    Ok(fixtures)
}

/// Small deterministic random numbers (xorshift), so fixtures are identical on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const WORDS: &[&str] = &[
    "the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "render", "tile", "engine",
    "scroll", "crisp", "sharp", "zoom", "page", "layout", "vector", "kraken", "smooth", "text",
];

fn text_page(number: usize, rng: &mut Rng) -> Vec<u8> {
    let mut ops = String::new();
    let _ = writeln!(ops, "q 0.85 0.9 1 rg 36 780 523 40 re f Q");
    let _ = writeln!(ops, "BT /F2 20 Tf 44 792 Td (Chapter {number}) Tj ET");
    for line in 0..52 {
        let words: Vec<&str> = (0..14)
            .map(|_| WORDS[rng.below(WORDS.len() as u64) as usize])
            .collect();
        let _ = writeln!(
            ops,
            "BT /F1 10 Tf 44 {} Td ({}) Tj ET",
            750 - line * 13,
            words.join(" ")
        );
    }
    let _ = writeln!(ops, "BT /F2 12 Tf 280 30 Td (Page {number}) Tj ET");
    ops.into_bytes()
}

/// About 20,000 line segments as a random walk, like a dense technical drawing.
fn vector_page(rng: &mut Rng) -> Vec<u8> {
    let mut ops = String::from("q 0.3 w\n");
    for path in 0..200 {
        let (mut x, mut y) = (rng.below(595) as i64, rng.below(842) as i64);
        let _ = write!(
            ops,
            "{} {} {} RG {x} {y} m",
            path % 3 / 2,
            path % 5 / 4,
            path % 7 / 6
        );
        for _ in 0..100 {
            x = (x + rng.below(41) as i64 - 20).clamp(0, 595);
            y = (y + rng.below(41) as i64 - 20).clamp(0, 842);
            let _ = write!(ops, " {x} {y} l");
        }
        ops.push_str(" S\n");
    }
    ops.push_str("Q\n");
    ops.into_bytes()
}

/// An uncompressed RGB image with gradients and noise, so it cannot be skipped cheaply.
fn image(width: usize, height: usize) -> (usize, usize, Vec<u8>) {
    let mut rng = Rng(42);
    let mut pixels = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        for x in 0..width {
            let noise = rng.below(32) as usize;
            pixels.push(((x * 255 / width + noise) % 256) as u8);
            pixels.push(((y * 255 / height + noise) % 256) as u8);
            pixels.push((((x + y) * 255 / (width + height) + noise) % 256) as u8);
        }
    }
    (width, height, pixels)
}

/// Writes a PDF with one content stream per page. Fonts F1 (Times) and F2 (Helvetica Bold)
/// and, if given, the image Im1 are available to every page.
fn write_pdf(
    path: &Path,
    pages: Vec<Vec<u8>>,
    image: Option<(usize, usize, Vec<u8>)>,
) -> std::io::Result<()> {
    let mut objects: Vec<Vec<u8>> = Vec::new();
    let add = |body: Vec<u8>, objects: &mut Vec<Vec<u8>>| {
        objects.push(body);
        objects.len()
    };
    let f1 = add(
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Times-Roman >>".to_vec(),
        &mut objects,
    );
    let f2 = add(
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>".to_vec(),
        &mut objects,
    );
    let image_ref = image.map(|(w, h, data)| {
        let mut body = format!(
            "<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceRGB \
             /BitsPerComponent 8 /Length {} >>\nstream\n",
            data.len()
        )
        .into_bytes();
        body.extend_from_slice(&data);
        body.extend_from_slice(b"\nendstream");
        add(body, &mut objects)
    });
    let resources = match image_ref {
        Some(im) => {
            format!("<< /Font << /F1 {f1} 0 R /F2 {f2} 0 R >> /XObject << /Im1 {im} 0 R >> >>")
        }
        None => format!("<< /Font << /F1 {f1} 0 R /F2 {f2} 0 R >> >>"),
    };
    let pages_id = objects.len() + 1;
    objects.push(Vec::new()); // the page tree, filled in below
    let mut kids = Vec::new();
    for content in pages {
        let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        stream.extend_from_slice(&content);
        stream.extend_from_slice(b"\nendstream");
        let content_id = add(stream, &mut objects);
        let page = format!(
            "<< /Type /Page /Parent {pages_id} 0 R /MediaBox [0 0 595 842] /Resources {resources} \
             /Contents {content_id} 0 R >>"
        );
        kids.push(add(page.into_bytes(), &mut objects));
    }
    let kids_list: Vec<String> = kids.iter().map(|k| format!("{k} 0 R")).collect();
    objects[pages_id - 1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids_list.join(" "),
        kids.len()
    )
    .into_bytes();
    let catalog = add(
        format!("<< /Type /Catalog /Pages {pages_id} 0 R >>").into_bytes(),
        &mut objects,
    );

    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root {catalog} 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    std::fs::write(path, out)
}
