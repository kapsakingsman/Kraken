//! The render worker process: its own PDFium, driven by a [`crate::RenderPool`] in the main
//! process over stdin/stdout (see [`crate::protocol`]).
//!
//! PDFium can only be used from one thread per process, so a second process is the way to
//! render on a second CPU core. A PDF that crashes PDFium also only takes down the worker.

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::protocol::{self, FromWorker, ToWorker};
use crate::{DocId, Engine, EngineConfig, EngineError, TileRequest};

/// Runs a worker until the pool closes its stdin. Call this first thing in `main` when the
/// process was started as a worker, before any window is created.
pub fn run_worker() -> io::Result<()> {
    let out = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let engine = Engine::start(EngineConfig::default())
        .map_err(|e| io::Error::other(format!("starting PDFium: {e}")))?;
    // The pool's document ids, mapped to this engine's own.
    let docs: Arc<Mutex<HashMap<DocId, DocId>>> = Arc::default();

    // Finished tiles go straight back, translated to the pool's document ids.
    let forward = {
        // A clone of the receiver, not of the engine: the loop ends when the engine stops.
        let results = engine.results().clone();
        let out = Arc::clone(&out);
        let docs = Arc::clone(&docs);
        thread::spawn(move || {
            for result in results {
                let pool_doc = {
                    let docs = docs.lock().expect("worker doc map");
                    docs.iter()
                        .find(|(_, local)| **local == result.key.doc)
                        .map(|(pool, _)| *pool)
                };
                // A tile of a document closed meanwhile: nobody wants it.
                let Some(pool_doc) = pool_doc else { continue };
                let key = crate::TileKey {
                    doc: pool_doc,
                    ..result.key
                };
                let message = match result.tile {
                    Ok(tile) => FromWorker::Tile {
                        key,
                        generation: result.generation,
                        width: tile.width,
                        height: tile.height,
                        draft: tile.draft,
                        render_time: tile.render_time,
                        rgba: tile.rgba,
                    },
                    Err(e) => FromWorker::Failed {
                        key,
                        generation: result.generation,
                        cancelled: matches!(e, EngineError::Cancelled),
                        message: e.to_string(),
                    },
                };
                let mut out = out.lock().expect("worker stdout");
                if protocol::write_from_worker(&mut *out, &message).is_err() {
                    return; // the pool is gone
                }
            }
        })
    };

    let mut input = BufReader::new(io::stdin().lock());
    while let Some(message) = protocol::read_to_worker(&mut input)? {
        match message {
            ToWorker::Open {
                doc,
                path,
                password,
            } => {
                let error = match engine.open(&path, password.as_deref()) {
                    Ok(info) => {
                        docs.lock().expect("worker doc map").insert(doc, info.id);
                        None
                    }
                    Err(e) => Some(e.to_string()),
                };
                let mut out = out.lock().expect("worker stdout");
                protocol::write_from_worker(&mut *out, &FromWorker::Opened { doc, error })?;
            }
            ToWorker::Close(doc) => {
                if let Some(local) = docs.lock().expect("worker doc map").remove(&doc) {
                    engine.close(local);
                }
            }
            ToWorker::Wanted {
                generation,
                requests,
            } => {
                let docs = docs.lock().expect("worker doc map");
                let requests: Vec<TileRequest> = requests
                    .into_iter()
                    .filter_map(|r| {
                        let local = *docs.get(&r.key.doc)?;
                        Some(TileRequest {
                            key: crate::TileKey {
                                doc: local,
                                ..r.key
                            },
                            ..r
                        })
                    })
                    .collect();
                engine.set_wanted(generation, requests, None);
            }
            ToWorker::Trim => engine.trim(),
        }
    }
    // The pool closed stdin: stop. Dropping the engine ends its thread and the forwarder.
    drop(engine);
    let _ = forward.join();
    out.lock().expect("worker stdout").flush()
}
