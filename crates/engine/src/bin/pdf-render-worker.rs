//! A standalone render worker, for tests and tools. The app runs itself as its worker
//! (`kraken-pdf --render-worker`) instead, so there is only one executable to ship.

fn main() -> std::io::Result<()> {
    pdf_engine::run_worker()
}
