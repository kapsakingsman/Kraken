//! Messages between the render pool and its worker processes, sent over the worker's stdin
//! and stdout. Each message is a little-endian `u32` length followed by that many bytes:
//! a tag byte and the fields in order. Both ends are the same executable, so the format
//! needs no versioning.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::{DocId, Quality, Scale, TileKey, TileRequest};

/// Upper bound on one message, far above a 512×512 tile (1 MB): a corrupt length must not
/// make the reader allocate gigabytes.
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// From the pool to a worker.
#[derive(Debug)]
pub enum ToWorker {
    Open {
        doc: DocId,
        path: PathBuf,
        password: Option<String>,
    },
    Close(DocId),
    /// Replaces what the worker should render (see `Engine::set_wanted`).
    Wanted {
        generation: u64,
        requests: Vec<TileRequest>,
    },
    /// Free caches while idle.
    Trim,
}

/// From a worker to the pool.
#[derive(Debug)]
pub enum FromWorker {
    Opened {
        doc: DocId,
        error: Option<String>,
    },
    Tile {
        key: TileKey,
        generation: u64,
        width: u32,
        height: u32,
        draft: bool,
        render_time: Duration,
        rgba: Vec<u8>,
    },
    Failed {
        key: TileKey,
        generation: u64,
        cancelled: bool,
        message: String,
    },
}

pub fn write_to_worker(out: &mut impl Write, message: &ToWorker) -> io::Result<()> {
    let mut b = Vec::new();
    match message {
        ToWorker::Open {
            doc,
            path,
            password,
        } => {
            b.push(1);
            put_u64(&mut b, doc.0);
            put_bytes(&mut b, path.as_os_str().as_encoded_bytes());
            match password {
                Some(p) => {
                    b.push(1);
                    put_bytes(&mut b, p.as_bytes());
                }
                None => b.push(0),
            }
        }
        ToWorker::Close(doc) => {
            b.push(2);
            put_u64(&mut b, doc.0);
        }
        ToWorker::Wanted {
            generation,
            requests,
        } => {
            b.push(3);
            put_u64(&mut b, *generation);
            put_u32(&mut b, requests.len() as u32);
            for r in requests {
                put_key(&mut b, &r.key);
                put_u64(&mut b, r.generation);
                put_u32(&mut b, r.priority);
                b.push(quality_byte(r.quality));
            }
        }
        ToWorker::Trim => b.push(4),
    }
    frame(out, &b)
}

pub fn read_to_worker(input: &mut impl Read) -> io::Result<Option<ToWorker>> {
    let Some(b) = read_frame(input)? else {
        return Ok(None);
    };
    let mut r = Reader { b: &b, at: 0 };
    let message = match r.u8()? {
        1 => {
            let doc = DocId(r.u64()?);
            let path_bytes = r.bytes()?.to_vec();
            // SAFETY: the bytes come from `as_encoded_bytes` in `write_to_worker`, written by
            // this same executable on this same machine.
            let path = PathBuf::from(unsafe {
                std::ffi::OsString::from_encoded_bytes_unchecked(path_bytes)
            });
            let password = match r.u8()? {
                0 => None,
                _ => Some(String::from_utf8_lossy(r.bytes()?).into_owned()),
            };
            ToWorker::Open {
                doc,
                path,
                password,
            }
        }
        2 => ToWorker::Close(DocId(r.u64()?)),
        3 => {
            let generation = r.u64()?;
            let count = r.u32()? as usize;
            let mut requests = Vec::with_capacity(count.min(4096));
            for _ in 0..count {
                let key = r.key()?;
                requests.push(TileRequest {
                    key,
                    generation: r.u64()?,
                    priority: r.u32()?,
                    quality: quality_from(r.u8()?)?,
                });
            }
            ToWorker::Wanted {
                generation,
                requests,
            }
        }
        4 => ToWorker::Trim,
        tag => return Err(invalid(format!("unknown message {tag}"))),
    };
    Ok(Some(message))
}

pub fn write_from_worker(out: &mut impl Write, message: &FromWorker) -> io::Result<()> {
    let mut b = Vec::new();
    match message {
        FromWorker::Opened { doc, error } => {
            b.push(1);
            put_u64(&mut b, doc.0);
            put_bytes(&mut b, error.as_deref().unwrap_or("").as_bytes());
            b.push(u8::from(error.is_some()));
        }
        FromWorker::Tile {
            key,
            generation,
            width,
            height,
            draft,
            render_time,
            rgba,
        } => {
            b.reserve(rgba.len() + 64);
            b.push(2);
            put_key(&mut b, key);
            put_u64(&mut b, *generation);
            put_u32(&mut b, *width);
            put_u32(&mut b, *height);
            b.push(u8::from(*draft));
            put_u64(&mut b, render_time.as_micros() as u64);
            put_bytes(&mut b, rgba);
        }
        FromWorker::Failed {
            key,
            generation,
            cancelled,
            message,
        } => {
            b.push(3);
            put_key(&mut b, key);
            put_u64(&mut b, *generation);
            b.push(u8::from(*cancelled));
            put_bytes(&mut b, message.as_bytes());
        }
    }
    frame(out, &b)
}

pub fn read_from_worker(input: &mut impl Read) -> io::Result<Option<FromWorker>> {
    let Some(b) = read_frame(input)? else {
        return Ok(None);
    };
    let mut r = Reader { b: &b, at: 0 };
    let message = match r.u8()? {
        1 => {
            let doc = DocId(r.u64()?);
            let text = String::from_utf8_lossy(r.bytes()?).into_owned();
            let error = (r.u8()? != 0).then_some(text);
            FromWorker::Opened { doc, error }
        }
        2 => FromWorker::Tile {
            key: r.key()?,
            generation: r.u64()?,
            width: r.u32()?,
            height: r.u32()?,
            draft: r.u8()? != 0,
            render_time: Duration::from_micros(r.u64()?),
            rgba: r.bytes()?.to_vec(),
        },
        3 => FromWorker::Failed {
            key: r.key()?,
            generation: r.u64()?,
            cancelled: r.u8()? != 0,
            message: String::from_utf8_lossy(r.bytes()?).into_owned(),
        },
        tag => return Err(invalid(format!("unknown message {tag}"))),
    };
    Ok(Some(message))
}

fn frame(out: &mut impl Write, body: &[u8]) -> io::Result<()> {
    out.write_all(&(body.len() as u32).to_le_bytes())?;
    out.write_all(body)?;
    out.flush()
}

/// Reads one message body; `None` when the other side closed the pipe between messages.
fn read_frame(input: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(invalid(format!("message of {len} bytes")));
    }
    let mut body = vec![0u8; len];
    input.read_exact(&mut body)?;
    Ok(Some(body))
}

fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put_bytes(b: &mut Vec<u8>, v: &[u8]) {
    put_u32(b, v.len() as u32);
    b.extend_from_slice(v);
}

fn put_key(b: &mut Vec<u8>, key: &TileKey) {
    put_u64(b, key.doc.0);
    put_u32(b, key.page);
    put_u32(b, key.scale.to_bits());
    put_u32(b, key.tx);
    put_u32(b, key.ty);
    put_u32(b, key.size);
}

fn quality_byte(q: Quality) -> u8 {
    match q {
        Quality::Preview => 0,
        Quality::Sharp => 1,
        Quality::Final => 2,
    }
}

fn quality_from(v: u8) -> io::Result<Quality> {
    Ok(match v {
        0 => Quality::Preview,
        1 => Quality::Sharp,
        2 => Quality::Final,
        _ => return Err(invalid(format!("unknown quality {v}"))),
    })
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&end| end <= self.b.len())
            .ok_or_else(|| invalid("message too short".into()))?;
        let bytes = &self.b[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn key(&mut self) -> io::Result<TileKey> {
        Ok(TileKey {
            doc: DocId(self.u64()?),
            page: self.u32()?,
            scale: Scale::from_bits(self.u32()?),
            tx: self.u32()?,
            ty: self.u32()?,
            size: self.u32()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> TileKey {
        TileKey {
            doc: DocId(7),
            page: 3,
            scale: Scale::from_px_per_pt(1.5),
            tx: 2,
            ty: 5,
            size: crate::geometry::SMALL_TILE_SIZE,
        }
    }

    #[test]
    fn messages_to_a_worker_round_trip() {
        let messages = [
            ToWorker::Open {
                doc: DocId(1),
                path: PathBuf::from("C:/dir with spaces/ünïcode.pdf"),
                password: Some("secret".into()),
            },
            ToWorker::Close(DocId(9)),
            ToWorker::Wanted {
                generation: 42,
                requests: vec![TileRequest {
                    key: key(),
                    generation: 41,
                    priority: 1003,
                    quality: Quality::Final,
                }],
            },
            ToWorker::Trim,
        ];
        let mut pipe = Vec::new();
        for m in &messages {
            write_to_worker(&mut pipe, m).unwrap();
        }
        let mut input = pipe.as_slice();
        for m in &messages {
            let read = read_to_worker(&mut input).unwrap().unwrap();
            assert_eq!(format!("{read:?}"), format!("{m:?}"));
        }
        assert!(read_to_worker(&mut input).unwrap().is_none(), "end of pipe");
    }

    #[test]
    fn messages_from_a_worker_round_trip() {
        let messages = [
            FromWorker::Opened {
                doc: DocId(1),
                error: None,
            },
            FromWorker::Opened {
                doc: DocId(2),
                error: Some("file not found".into()),
            },
            FromWorker::Tile {
                key: key(),
                generation: 5,
                width: 3,
                height: 1,
                draft: true,
                render_time: Duration::from_micros(1234),
                rgba: vec![1; 12],
            },
            FromWorker::Failed {
                key: key(),
                generation: 6,
                cancelled: true,
                message: "rendering was cancelled".into(),
            },
        ];
        let mut pipe = Vec::new();
        for m in &messages {
            write_from_worker(&mut pipe, m).unwrap();
        }
        let mut input = pipe.as_slice();
        for m in &messages {
            let read = read_from_worker(&mut input).unwrap().unwrap();
            assert_eq!(format!("{read:?}"), format!("{m:?}"));
        }
        assert!(read_from_worker(&mut input).unwrap().is_none());
    }

    #[test]
    fn corrupt_input_is_an_error_not_a_panic_or_a_huge_allocation() {
        let huge = u32::MAX.to_le_bytes();
        assert!(read_from_worker(&mut huge.as_slice()).is_err());
        let truncated = [5, 0, 0, 0, 2, 1];
        assert!(read_from_worker(&mut truncated.as_slice()).is_err());
        let unknown_tag = [1, 0, 0, 0, 99];
        assert!(read_to_worker(&mut unknown_tag.as_slice()).is_err());
    }
}
