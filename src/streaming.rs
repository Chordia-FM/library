//! HTTP Range streaming (RFC 7233).
//!
//! `serve_range` opens a file and streams its bytes unaltered with the correct `Accept-Ranges`,
//! `Content-Range`, `ETag`, and `Content-Type` headers. It serves both the bit-perfect `Original`
//! tier (the source file) and the on-disk transcoded lower tiers (see [`crate::transcode`]) - the
//! caller picks the file and codec.

use std::path::Path;

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

/// Wrap a reader as a response body, optionally rate-limited to `max_kbps`.
fn reader_body<R>(reader: R, max_kbps: Option<u32>) -> Body
where
    R: AsyncRead + Send + Unpin + 'static,
{
    match max_kbps {
        Some(kbps) if kbps > 0 => Body::from_stream(throttled(reader, kbps)),
        _ => Body::from_stream(ReaderStream::new(reader)),
    }
}

/// Pace a reader's output to `kbps` by sleeping so cumulative bytes never outrun the target rate.
fn throttled<R>(
    mut reader: R,
    kbps: u32,
) -> impl futures_core::Stream<Item = std::io::Result<Bytes>>
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let bytes_per_sec = ((kbps as u64) * 1024 / 8).max(1);
    async_stream::stream! {
        let started = tokio::time::Instant::now();
        let mut sent: u64 = 0;
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            let n = match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => { yield Err(e); break; }
            };
            sent += n as u64;
            let target = std::time::Duration::from_secs_f64(sent as f64 / bytes_per_sec as f64);
            let elapsed = started.elapsed();
            if target > elapsed {
                tokio::time::sleep(target - elapsed).await;
            }
            yield Ok(Bytes::copy_from_slice(&buf[..n]));
        }
    }
}

/// Parse `bytes=start-end` from a `Range` header value.
fn parse_range(header: &str, total: u64) -> Option<(u64, u64)> {
    let s = header.strip_prefix("bytes=")?;
    let (start_str, end_str) = s.split_once('-')?;

    let start: u64 = start_str.parse().ok()?;
    let end: u64 = if end_str.is_empty() {
        total.saturating_sub(1)
    } else {
        end_str.parse().ok()?
    };

    if start > end || end >= total {
        return None;
    }
    Some((start, end))
}

fn content_type_for(codec: &str, path: &Path) -> &'static str {
    match codec {
        "flac" => "audio/flac",
        "mp3" => "audio/mpeg",
        "aac" => "audio/mp4",
        "alac" => "audio/mp4",
        "vorbis" => "audio/ogg",
        "opus" => "audio/ogg; codecs=opus",
        "pcm" | "wav" => "audio/wav",
        "aiff" => "audio/aiff",
        _ => {
            // Fallback: guess from extension.
            let guess = mime_guess::from_path(path);
            guess.first_raw().unwrap_or("application/octet-stream")
        }
    }
}

/// Stream the original file bytes, honoring any `Range` header.
///
/// Returns 206 Partial Content if a valid Range was supplied, 200 OK otherwise.
pub async fn serve_range(
    path: &Path,
    content_hash: &str,
    codec: &str,
    request_headers: &HeaderMap,
    max_kbps: Option<u32>,
) -> Result<Response, anyhow::Error> {
    let mut file = File::open(path).await?;
    let metadata = file.metadata().await?;
    let total = metadata.len();

    let etag = format!("\"{content_hash}\"");
    let content_type = content_type_for(codec, path);

    // Check If-None-Match for conditional GET.
    if let Some(inm) = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if inm == etag {
            return Ok((StatusCode::NOT_MODIFIED, ()).into_response());
        }
    }

    let range_header = request_headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok());

    if let Some(range_str) = range_header {
        match parse_range(range_str, total) {
            Some((start, end)) => {
                let length = end - start + 1;
                file.seek(std::io::SeekFrom::Start(start)).await?;
                let limited = file.take(length);

                let response = Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::CONTENT_LENGTH, length.to_string())
                    .header(header::ACCEPT_RANGES, "bytes")
                    .header(
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{total}"),
                    )
                    .header(header::ETAG, &etag)
                    .header(header::CACHE_CONTROL, "no-store")
                    .body(reader_body(limited, max_kbps))?;

                Ok(response)
            }
            None => {
                // Invalid/unsatisfiable range.
                Ok(Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{total}"))
                    .body(Body::empty())?)
            }
        }
    } else {
        // Full response.
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CONTENT_LENGTH, total.to_string())
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::ETAG, &etag)
            .header(header::CACHE_CONTROL, "no-store")
            .body(reader_body(file, max_kbps))?;

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;
    use std::io::Cursor;
    use std::path::Path;
    use std::pin::pin;

    use futures_core::Stream;

    use super::{content_type_for, parse_range, throttled};

    #[test]
    fn parse_range_handles_closed_open_and_invalid() {
        // Closed range.
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        // Open-ended range clamps to the last byte.
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        // Whole file.
        assert_eq!(parse_range("bytes=0-", 1000), Some((0, 999)));

        // start > end is rejected.
        assert_eq!(parse_range("bytes=200-100", 1000), None);
        // end past EOF is rejected.
        assert_eq!(parse_range("bytes=0-1000", 1000), None);
        // Missing unit prefix.
        assert_eq!(parse_range("0-99", 1000), None);
        // Suffix ranges (`bytes=-500`) are not supported by this server.
        assert_eq!(parse_range("bytes=-500", 1000), None);
    }

    #[test]
    fn content_type_maps_known_codecs_and_falls_back() {
        let p = Path::new("song.bin");
        assert_eq!(content_type_for("flac", p), "audio/flac");
        assert_eq!(content_type_for("mp3", p), "audio/mpeg");
        assert_eq!(content_type_for("opus", p), "audio/ogg; codecs=opus");
        assert_eq!(content_type_for("alac", p), "audio/mp4");
        // Unknown codec falls back to a guess from the extension.
        assert_eq!(content_type_for("???", Path::new("a.mp3")), "audio/mpeg");
        // Unknown codec + no extension to guess from → octet-stream.
        assert_eq!(
            content_type_for("???", Path::new("noextension")),
            "application/octet-stream"
        );
    }

    /// Drain a `throttled` stream into a single buffer (no extra stream-combinator deps).
    async fn collect<S>(stream: S) -> Vec<u8>
    where
        S: Stream<Item = std::io::Result<bytes::Bytes>>,
    {
        let mut stream = pin!(stream);
        let mut out = Vec::new();
        while let Some(item) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            out.extend_from_slice(&item.expect("chunk"));
        }
        out
    }

    #[tokio::test]
    async fn throttle_reproduces_all_bytes_across_chunk_boundaries() {
        // 70 KiB spans three 32 KiB read buffers, exercising the multi-chunk path.
        let data: Vec<u8> = (0..70 * 1024).map(|i| (i % 251) as u8).collect();
        // A high rate keeps the pacing sleeps negligible so the test stays fast.
        let out = collect(throttled(Cursor::new(data.clone()), 1_000_000)).await;
        assert_eq!(
            out, data,
            "throttled stream must reproduce the source byte-for-byte"
        );
    }

    #[tokio::test]
    async fn throttle_handles_empty_input() {
        let out = collect(throttled(Cursor::new(Vec::new()), 96)).await;
        assert!(out.is_empty());
    }
}
