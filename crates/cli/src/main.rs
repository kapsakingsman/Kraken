//! Command-line tool to check render quality and speed without the UI.
//!
//! Everything goes through the same engine and tile pipeline the app uses, so a PNG from
//! `render` shows exactly the pixels the app will put on screen at that zoom.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use pdf_engine::geometry::{page_px_size, tile_grid, tile_rect};
use pdf_engine::{
    DocInfo, Engine, EngineConfig, Scale, TILE_SIZE, Tile, TileKey, TileRect, TileRequest,
};

#[derive(Parser)]
#[command(version, about = "Render and benchmark PDFs with the PDF engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Password for protected PDFs.
    #[arg(long, global = true)]
    password: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Show the page count and page sizes.
    Info { file: PathBuf },

    /// Render one page to a PNG.
    Render {
        file: PathBuf,

        /// Page number, starting at 1.
        #[arg(long, default_value_t = 1)]
        page: u32,

        #[command(flatten)]
        view: View,

        /// Output file. Defaults to out/<name>-p<page>-<zoom>.png
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Measure how long pages take to render.
    Bench {
        file: PathBuf,

        #[command(flatten)]
        view: View,

        /// Only measure the first N pages.
        #[arg(long)]
        pages: Option<u32>,
    },
}

#[derive(clap::Args)]
struct View {
    /// Zoom in percent, the same number as Acrobat's zoom box.
    #[arg(long, default_value_t = 100.0)]
    zoom: f32,

    /// Windows display scaling (Settings > System > Display > Scale), e.g. 1.5 for 150%.
    #[arg(long, default_value_t = 1.0)]
    display_scale: f32,
}

impl View {
    fn scale(&self) -> Result<Scale> {
        if !(1.0..=6400.0).contains(&self.zoom) {
            bail!("--zoom must be between 1 and 6400");
        }
        if !(0.5..=4.0).contains(&self.display_scale) {
            bail!("--display-scale must be between 0.5 and 4");
        }
        Ok(Scale::from_zoom(self.zoom, self.display_scale))
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let engine = Engine::start(EngineConfig::default())?;
    let open = |file: &Path| -> Result<(DocInfo, Duration)> {
        let started = Instant::now();
        let doc = engine
            .open(file, cli.password.as_deref())
            .with_context(|| format!("opening {}", file.display()))?;
        Ok((doc, started.elapsed()))
    };

    match &cli.command {
        Command::Info { file } => {
            let (doc, open_time) = open(file)?;
            info(&doc, file, open_time);
        }
        Command::Render {
            file,
            page,
            view,
            out,
        } => {
            let scale = view.scale()?;
            let (doc, _) = open(file)?;
            let page_count = doc.page_sizes.len() as u32;
            if *page == 0 || *page > page_count {
                bail!("page {page} does not exist; this PDF has {page_count} pages");
            }
            let out = out
                .clone()
                .unwrap_or_else(|| default_output(file, *page, view.zoom));
            render(&engine, &doc, *page - 1, scale, &out)?;
        }
        Command::Bench { file, view, pages } => {
            let scale = view.scale()?;
            let (doc, open_time) = open(file)?;
            bench(&engine, &doc, file, view, scale, open_time, *pages)?;
        }
    }
    Ok(())
}

fn info(doc: &DocInfo, file: &Path, open_time: Duration) {
    println!("{}", file.display());
    println!("  pages:  {}", doc.page_sizes.len());
    println!("  opened: {}", ms(open_time));
    for (index, size) in doc.page_sizes.iter().enumerate().take(10) {
        println!(
            "  page {:>4}: {:.0} x {:.0} pt ({:.2} x {:.2} in)",
            index + 1,
            size.width_pt,
            size.height_pt,
            size.width_pt / 72.0,
            size.height_pt / 72.0
        );
    }
    if doc.page_sizes.len() > 10 {
        println!("  ... {} more", doc.page_sizes.len() - 10);
    }
}

fn render(engine: &Engine, doc: &DocInfo, page: u32, scale: Scale, out: &Path) -> Result<()> {
    let (width, height) = page_px_size(doc.page_sizes[page as usize], scale);
    if width as u64 * height as u64 > 250_000_000 {
        bail!("{width} x {height} pixels is too large for a PNG; use a lower --zoom");
    }
    let mut image = vec![0u8; width as usize * height as usize * 4];
    let stats = render_page(engine, doc, page, scale, |rect, tile| {
        let row_bytes = rect.width as usize * 4;
        for (row, src) in tile.rgba.chunks_exact(row_bytes).enumerate() {
            let start = ((rect.y as usize + row) * width as usize + rect.x as usize) * 4;
            image[start..start + row_bytes].copy_from_slice(src);
        }
    })?;
    write_png(out, width, height, &image)?;

    println!(
        "page {} -> {} ({width} x {height} px, {} tiles, {})",
        page + 1,
        out.display(),
        stats.tile_times.len(),
        ms(stats.wall)
    );
    Ok(())
}

fn bench(
    engine: &Engine,
    doc: &DocInfo,
    file: &Path,
    view: &View,
    scale: Scale,
    open_time: Duration,
    limit: Option<u32>,
) -> Result<()> {
    let page_count = doc.page_sizes.len() as u32;
    let pages = limit.map_or(page_count, |n| n.min(page_count));
    println!("{}", file.display());
    println!(
        "  zoom {}% x display scale {} = {:.3} px/pt, tiles {TILE_SIZE} px",
        view.zoom,
        view.display_scale,
        scale.px_per_pt()
    );
    println!("  opened in {} ({page_count} pages)", ms(open_time));

    let started = Instant::now();
    let mut all_tiles = Vec::new();
    let mut per_page = Vec::new();
    for page in 0..pages {
        let stats = render_page(engine, doc, page, scale, |_, _| {})?;
        if page == 0 {
            println!("  first page ready in {}", ms(open_time + stats.wall));
        }
        let slowest = stats.tile_times.iter().max().copied().unwrap_or_default();
        per_page.push((page, stats.wall, stats.tile_times.len(), slowest));
        all_tiles.extend(stats.tile_times);
    }
    let total = started.elapsed();

    all_tiles.sort();
    println!(
        "  rendered {pages} pages in {} ({:.1} pages/s)",
        ms(total),
        pages as f64 / total.as_secs_f64()
    );
    println!(
        "  per tile: p50 {}, p95 {}, max {} ({} tiles)",
        ms(percentile(&all_tiles, 0.50)),
        ms(percentile(&all_tiles, 0.95)),
        ms(all_tiles.last().copied().unwrap_or_default()),
        all_tiles.len()
    );
    per_page.sort_by_key(|p| std::cmp::Reverse(p.1));
    println!("  slowest pages:");
    for (page, wall, tiles, slowest) in per_page.iter().take(5) {
        println!(
            "    page {:>4}: {} ({tiles} tiles, slowest tile {})",
            page + 1,
            ms(*wall),
            ms(*slowest)
        );
    }
    Ok(())
}

struct PageStats {
    wall: Duration,
    tile_times: Vec<Duration>,
}

/// Requests every tile of a page and hands each finished tile to `on_tile`.
fn render_page(
    engine: &Engine,
    doc: &DocInfo,
    page: u32,
    scale: Scale,
    mut on_tile: impl FnMut(TileRect, &Tile),
) -> Result<PageStats> {
    let page_px = page_px_size(doc.page_sizes[page as usize], scale);
    let (cols, rows) = tile_grid(page_px);
    let started = Instant::now();
    for ty in 0..rows {
        for tx in 0..cols {
            engine.request_tile(TileRequest {
                key: TileKey {
                    doc: doc.id,
                    page,
                    scale,
                    tx,
                    ty,
                },
                generation: 0,
                priority: ty * cols + tx,
            });
        }
    }

    let mut tile_times = Vec::with_capacity((cols * rows) as usize);
    for _ in 0..cols * rows {
        let result = engine
            .results()
            .recv()
            .context("engine stopped unexpectedly")?;
        let (tx, ty) = (result.key.tx, result.key.ty);
        let tile = result
            .tile
            .with_context(|| format!("page {} tile ({tx}, {ty})", page + 1))?;
        let rect = tile_rect(page_px, tx, ty).context("engine returned a tile outside the page")?;
        on_tile(rect, &tile);
        tile_times.push(tile.render_time);
    }
    Ok(PageStats {
        wall: started.elapsed(),
        tile_times,
    })
}

fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    // PDFium renders sRGB colors; tagging the file keeps image viewers from shifting them.
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    writer.finish()?;
    Ok(())
}

fn default_output(file: &Path, page: u32, zoom: f32) -> PathBuf {
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "page".into());
    PathBuf::from("out").join(format!("{stem}-p{page}-{zoom}.png"))
}

fn percentile(sorted: &[Duration], q: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}
