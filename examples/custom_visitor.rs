#![allow(clippy::wildcard_imports)]
//! Implement a custom `StreamVisitor` for state that doesn't fit a closure.
//!
//! The bundled visitors under `tokio_process_tools::visitors` cover the common cases. Reach
//! for a custom `StreamVisitor` (sync) or `AsyncStreamVisitor` (async) when you need to:
//! maintain state across chunks, react explicitly to gap notifications, or produce an
//! output type the built-ins do not expose. If all you need is a side effect per chunk,
//! `InspectChunks` (or `ParseLines::inspect` for line work) is simpler.
//!
//! Pick `StreamVisitor` when `on_chunk` can return without `.await`-ing (counters, in-memory
//! state machines, channel `try_send`, simple synchronous I/O). Pick `AsyncStreamVisitor`
//! when chunk handling needs to await. Both traits require `Send + 'static` and both return
//! their final value via `into_output(self)` at the end of the consumer task's life.
//!
//! `on_gap` is invoked when one or more chunks were dropped between the previous and next
//! observed chunk (only possible under `lossy_without_backpressure()` overflow); visitors that
//! splice bytes across chunks should treat it as a reset signal. `on_eof` is skipped when
//! `on_chunk` returned `Next::Break`, on the assumption that an early-break visitor already
//! has its result; cancellation and abort skip `on_eof` for the same reason. Whichever exit
//! path was taken, `into_output` always runs, so `Consumer::wait()` and `cancel()` always
//! have a value to yield.
//!
//! For per-line state instead of per-chunk state, implement `LineVisitor` (or
//! `AsyncLineVisitor`) and feed it through `ParseLines::new(parsing_options, visitor)`.
//! The shape is the same: `on_line`, `on_gap`, `on_eof`, `into_output`.
//!
//! Run: `cargo run --example custom_visitor`

use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::*;

#[derive(Debug, Default)]
struct ChunkStats {
    bytes: usize,
    chunks: usize,
    gaps: usize,
}

impl StreamVisitor for ChunkStats {
    type Output = Self;

    fn on_chunk(&mut self, chunk: Chunk) -> Next {
        self.bytes += chunk.as_ref().len();
        self.chunks += 1;
        Next::Continue
    }

    fn on_gap(&mut self) {
        self.gaps += 1;
    }

    fn into_output(self) -> Self::Output {
        self
    }
}

#[tokio::main]
async fn main() {
    let mut process = Process::new(Command::new("ls"))
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .lossy_without_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("failed to spawn command");

    let counter = process
        .stdout()
        .consume(ChunkStats::default())
        .expect("no other consumer is attached yet");

    let _status = process
        .wait_for_completion(Duration::from_secs(30))
        .await
        .unwrap()
        .expect_completed("process should complete");

    let stats = counter.wait().await.unwrap();
    println!(
        "stdout: {} bytes across {} chunks ({} gaps)",
        stats.bytes, stats.chunks, stats.gaps,
    );
}
