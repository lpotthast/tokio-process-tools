use super::state::{ActiveSubscriber, ConfiguredShared, SubscriberId};
use crate::output_stream::event::{Chunk, StreamEvent};
use crate::output_stream::policy::{DeliveryGuarantee, ReplayRetention};
use crate::{NumBytes, StreamReadError};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc::error::{SendError, TrySendError};
use tokio::sync::watch;

fn record_for_later(
    shared: &ConfiguredShared,
    event: &StreamEvent,
    replay_retention: Option<ReplayRetention>,
) {
    match event {
        StreamEvent::Chunk(_) if replay_retention.is_some() => {}
        StreamEvent::Eof | StreamEvent::ReadError(_) => {}
        StreamEvent::Chunk(_) | StreamEvent::Gap => return,
    }

    let mut state = shared
        .state
        .lock()
        .expect("single-subscriber state poisoned");
    state.push_replay_event(event.clone(), replay_retention);
}

fn refresh_cached_active(
    cached_active: &mut Option<Arc<ActiveSubscriber>>,
    active_rx: &mut watch::Receiver<Option<Arc<ActiveSubscriber>>>,
) {
    cached_active.clone_from(&active_rx.borrow_and_update());
}

async fn send_reliable_event(
    mut event: StreamEvent,
    shared: &ConfiguredShared,
    cached_active: &mut Option<Arc<ActiveSubscriber>>,
    active_rx: &mut watch::Receiver<Option<Arc<ActiveSubscriber>>>,
    replay_retention: Option<ReplayRetention>,
) {
    record_for_later(shared, &event, replay_retention);

    let mut retried = false;
    loop {
        let Some(active) = cached_active.clone() else {
            return;
        };

        match active.sender.send(event).await {
            Ok(()) => return,
            Err(SendError(returned)) => {
                shared.clear_active_if_current(active.id);
                refresh_cached_active(cached_active, active_rx);
                event = returned;
                if retried {
                    return;
                }
                retried = true;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BestEffortSend {
    Delivered,
    Full,
    Inactive,
}

fn try_send_best_effort_event(
    mut event: StreamEvent,
    shared: &ConfiguredShared,
    cached_active: &mut Option<Arc<ActiveSubscriber>>,
    active_rx: &mut watch::Receiver<Option<Arc<ActiveSubscriber>>>,
    replay_retention: Option<ReplayRetention>,
    retry_on_closed: bool,
) -> BestEffortSend {
    record_for_later(shared, &event, replay_retention);

    let mut retried = false;
    loop {
        let Some(active) = cached_active.clone() else {
            return BestEffortSend::Inactive;
        };

        match active.sender.try_send(event) {
            Ok(()) => return BestEffortSend::Delivered,
            Err(TrySendError::Full(_returned)) => return BestEffortSend::Full,
            Err(TrySendError::Closed(returned)) => {
                shared.clear_active_if_current(active.id);
                refresh_cached_active(cached_active, active_rx);
                event = returned;
                if !retry_on_closed || retried {
                    return BestEffortSend::Inactive;
                }
                retried = true;
            }
        }
    }
}

#[derive(Debug, Default)]
struct BestEffortLag {
    subscriber_id: Option<SubscriberId>,
    lagged: usize,
    gap_pending: bool,
}

impl BestEffortLag {
    fn observe_active(&mut self, active: Option<&Arc<ActiveSubscriber>>) {
        let subscriber_id = active.map(|active| active.id);
        if self.subscriber_id != subscriber_id {
            self.subscriber_id = subscriber_id;
            self.lagged = 0;
            self.gap_pending = false;
        }
    }
}

fn log_if_lagged(lagged: &mut usize) {
    if *lagged > 0 {
        tracing::debug!(lagged = *lagged, "Stream reader is lagging behind");
        *lagged = 0;
    }
}

fn buffered_chunk_count(buffer_len: usize, chunk_size: NumBytes) -> usize {
    buffer_len.div_ceil(chunk_size.bytes())
}

async fn send_pending_gap_before_terminal(
    shared: &ConfiguredShared,
    cached_active: &mut Option<Arc<ActiveSubscriber>>,
    active_rx: &mut watch::Receiver<Option<Arc<ActiveSubscriber>>>,
    lag: &mut BestEffortLag,
) {
    if !lag.gap_pending {
        return;
    }

    let Some(active) = cached_active.clone() else {
        lag.observe_active(None);
        return;
    };

    match active.sender.send(StreamEvent::Gap).await {
        Ok(()) => {
            log_if_lagged(&mut lag.lagged);
            lag.gap_pending = false;
        }
        Err(SendError(_event)) => {
            shared.clear_active_if_current(active.id);
            refresh_cached_active(cached_active, active_rx);
            lag.observe_active(cached_active.as_ref());
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the reader keeps the read/chunk loop inline to preserve delivery invariants"
)]
pub(super) async fn read_chunked<R: AsyncRead + Unpin + Send + 'static>(
    mut read: R,
    shared: Arc<ConfiguredShared>,
    mut active_rx: watch::Receiver<Option<Arc<ActiveSubscriber>>>,
    chunk_size: NumBytes,
    delivery_guarantee: DeliveryGuarantee,
    replay_retention: Option<ReplayRetention>,
    stream_name: &'static str,
) {
    let mut buf = bytes::BytesMut::with_capacity(chunk_size.bytes());
    let mut cached_active = active_rx.borrow().clone();
    let mut best_effort_lag = BestEffortLag::default();
    best_effort_lag.observe_active(cached_active.as_ref());

    // A cached active sender does not need to poll shared active state while its receiver is
    // alive: the mpsc sender remains valid until that consumer drops its receiver. A closed
    // channel is the signal to refresh the cached active slot. Subscriber ids prevent a stale
    // sender failure from clearing a newer active subscriber.
    'outer: loop {
        let _ = buf.try_reclaim(chunk_size.bytes());

        let read_result = if cached_active.is_none() {
            tokio::select! {
                result = read.read_buf(&mut buf) => {
                    refresh_cached_active(&mut cached_active, &mut active_rx);
                    best_effort_lag.observe_active(cached_active.as_ref());
                    result
                }
                changed = active_rx.changed() => {
                    if changed.is_ok() {
                        refresh_cached_active(&mut cached_active, &mut active_rx);
                        best_effort_lag.observe_active(cached_active.as_ref());
                        continue 'outer;
                    }
                    read.read_buf(&mut buf).await
                }
            }
        } else {
            read.read_buf(&mut buf).await
        };

        match read_result {
            Ok(bytes_read) => {
                let is_eof = bytes_read == 0;

                if is_eof {
                    if matches!(delivery_guarantee, DeliveryGuarantee::BestEffort) {
                        send_pending_gap_before_terminal(
                            &shared,
                            &mut cached_active,
                            &mut active_rx,
                            &mut best_effort_lag,
                        )
                        .await;
                    }

                    send_reliable_event(
                        StreamEvent::Eof,
                        &shared,
                        &mut cached_active,
                        &mut active_rx,
                        replay_retention,
                    )
                    .await;
                    break;
                }

                while !buf.is_empty() {
                    match delivery_guarantee {
                        DeliveryGuarantee::ReliableForActiveSubscribers => {
                            let split_to = usize::min(chunk_size.bytes(), buf.len());
                            let chunk = Chunk(buf.split_to(split_to).freeze());
                            let event = StreamEvent::Chunk(chunk);

                            send_reliable_event(
                                event,
                                &shared,
                                &mut cached_active,
                                &mut active_rx,
                                replay_retention,
                            )
                            .await;
                        }
                        DeliveryGuarantee::BestEffort => {
                            best_effort_lag.observe_active(cached_active.as_ref());

                            if best_effort_lag.gap_pending {
                                match try_send_best_effort_event(
                                    StreamEvent::Gap,
                                    &shared,
                                    &mut cached_active,
                                    &mut active_rx,
                                    replay_retention,
                                    false,
                                ) {
                                    BestEffortSend::Delivered => {
                                        log_if_lagged(&mut best_effort_lag.lagged);
                                        best_effort_lag.gap_pending = false;
                                    }
                                    BestEffortSend::Full => {
                                        best_effort_lag.lagged +=
                                            buffered_chunk_count(buf.len(), chunk_size);
                                        buf.clear();
                                        tokio::task::yield_now().await;
                                        continue;
                                    }
                                    BestEffortSend::Inactive => {
                                        best_effort_lag.observe_active(cached_active.as_ref());
                                    }
                                }
                            }

                            let split_to = usize::min(chunk_size.bytes(), buf.len());
                            let chunk = Chunk(buf.split_to(split_to).freeze());
                            let event = StreamEvent::Chunk(chunk);

                            match try_send_best_effort_event(
                                event,
                                &shared,
                                &mut cached_active,
                                &mut active_rx,
                                replay_retention,
                                true,
                            ) {
                                BestEffortSend::Delivered => {
                                    log_if_lagged(&mut best_effort_lag.lagged);
                                }
                                BestEffortSend::Full => {
                                    best_effort_lag.lagged += 1;
                                    best_effort_lag.gap_pending = true;
                                    tokio::task::yield_now().await;
                                }
                                BestEffortSend::Inactive => {
                                    best_effort_lag.observe_active(cached_active.as_ref());
                                }
                            }
                        }
                    }
                }
            }
            Err(err) => {
                let err = StreamReadError::new(stream_name, err);
                tracing::warn!(error = %err, "Could not read from stream");
                send_reliable_event(
                    StreamEvent::ReadError(err),
                    &shared,
                    &mut cached_active,
                    &mut active_rx,
                    replay_retention,
                )
                .await;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::{ActiveSubscriber, ConfiguredShared};
    use super::*;
    use crate::output_stream::Next;
    use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
    use crate::output_stream::config::StreamConfig;
    use crate::output_stream::policy::DeliveryGuarantee;
    use crate::test_support::ReadErrorAfterBytes;
    use crate::{DEFAULT_READ_CHUNK_SIZE, LineParsingOptions, NumBytesExt, RawCollectionOptions};
    use assertr::prelude::*;
    use std::io::{self, Cursor};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::{AsyncWriteExt, ReadBuf};
    use tokio::sync::mpsc;
    use tokio::time::sleep;
    use tracing_test::traced_test;

    fn spawn_configured_reader<R>(
        read: R,
        delivery_guarantee: DeliveryGuarantee,
        read_chunk_size: NumBytes,
        max_buffered_chunks: usize,
    ) -> (tokio::task::JoinHandle<()>, mpsc::Receiver<StreamEvent>)
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel(max_buffered_chunks);
        let shared = Arc::new(ConfiguredShared::new());
        let active_rx = shared.subscribe_active();
        {
            let mut state = shared
                .state
                .lock()
                .expect("single-subscriber state poisoned");
            let id = state.attach_subscriber();
            shared
                .active_tx
                .send_replace(Some(Arc::new(ActiveSubscriber { id, sender })));
        }

        let stream_reader = tokio::spawn(read_chunked(
            read,
            shared,
            active_rx,
            read_chunk_size,
            delivery_guarantee,
            None,
            "custom",
        ));

        (stream_reader, receiver)
    }

    async fn wait_for_no_active_consumer(stream: &SingleSubscriberOutputStream) {
        let shared = Arc::clone(stream.configured_shared.as_ref().unwrap());
        for _ in 0..50 {
            {
                let state = shared
                    .state
                    .lock()
                    .expect("single-subscriber state poisoned");
                if state.active_id.is_none() {
                    return;
                }
            }
            sleep(Duration::from_millis(10)).await;
        }

        assert_that!(()).fail("active consumer did not detach");
    }

    #[derive(Debug)]
    struct AlwaysReadyBytes {
        remaining: usize,
        bytes_read: Arc<AtomicUsize>,
    }

    impl AlwaysReadyBytes {
        fn new(total_bytes: usize, bytes_read: Arc<AtomicUsize>) -> Self {
            Self {
                remaining: total_bytes,
                bytes_read,
            }
        }
    }

    impl AsyncRead for AlwaysReadyBytes {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            const BYTES: [u8; 1024] = [b'x'; 1024];

            if self.remaining == 0 {
                return Poll::Ready(Ok(()));
            }

            let len = self.remaining.min(buf.remaining()).min(BYTES.len());
            buf.put_slice(&BYTES[..len]);
            self.remaining -= len;
            self.bytes_read.fetch_add(len, Ordering::Relaxed);
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn configured_reader_does_not_terminate_when_first_read_can_fill_the_entire_bytes_mut_buffer()
     {
        let (read_half, mut write_half) = tokio::io::duplex(64);

        // Write more data than the configured chunk size so the initial read can fill the buffer.
        write_half.write_all(b"hello world").await.unwrap();
        write_half.flush().await.unwrap();

        let (stream_reader, mut rx) =
            spawn_configured_reader(read_half, DeliveryGuarantee::BestEffort, 2.bytes(), 64);

        drop(write_half);
        stream_reader.await.unwrap();

        let mut chunks = Vec::<String>::new();
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Chunk(chunk) => {
                    chunks.push(String::from_utf8_lossy(chunk.as_ref()).to_string());
                }
                StreamEvent::Gap => {}
                StreamEvent::Eof => break,
                StreamEvent::ReadError(err) => {
                    assert_that!(&err).fail(format_args!("unexpected read error: {err}"));
                }
            }
        }
        assert_that!(chunks).contains_exactly(["he", "ll", "o ", "wo", "rl", "d"]);
    }

    #[tokio::test]
    async fn configured_reader_sends_pending_gap_before_terminal_eof() {
        let read = Cursor::new(b"aabbcc".to_vec());
        let (stream_reader, mut rx) =
            spawn_configured_reader(read, DeliveryGuarantee::BestEffort, 2.bytes(), 1);

        match rx.recv().await.unwrap() {
            StreamEvent::Chunk(chunk) => {
                assert_that!(chunk.as_ref()).is_equal_to(b"aa".as_slice());
            }
            other => {
                assert_that!(&other).fail(format_args!("expected first chunk, got {other:?}"));
            }
        }

        let mut previous = None;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Eof => {
                    assert_that!(previous).is_equal_to(Some(StreamEvent::Gap));
                    break;
                }
                StreamEvent::Chunk(chunk) => {
                    assert_that!(chunk.as_ref()).fail("dropped chunks should not be delivered");
                }
                event @ StreamEvent::Gap => {
                    previous = Some(event);
                }
                StreamEvent::ReadError(err) => {
                    assert_that!(&err).fail(format_args!("unexpected read error: {err}"));
                }
            }
        }

        stream_reader.await.unwrap();
        assert_that!(rx.recv().await).is_none();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn configured_best_effort_yields_when_pending_gap_channel_is_full() {
        let total_bytes = 1024;
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let (stream_reader, mut rx) = spawn_configured_reader(
            AlwaysReadyBytes::new(total_bytes, Arc::clone(&bytes_read)),
            DeliveryGuarantee::BestEffort,
            1.bytes(),
            1,
        );

        match rx.recv().await.unwrap() {
            StreamEvent::Chunk(chunk) => {
                assert_that!(chunk.as_ref()).is_equal_to(b"x".as_slice());
            }
            other => {
                assert_that!(&other).fail(format_args!("expected first chunk, got {other:?}"));
            }
        }

        let observed = bytes_read.load(Ordering::Relaxed);
        assert_that!(observed < total_bytes)
            .with_detail_message(format!(
                "reader consumed all {total_bytes} bytes before yielding"
            ))
            .is_true();

        drop(rx);
        stream_reader.await.unwrap();
    }

    #[tokio::test]
    async fn configured_reader_sends_pending_gap_before_resumed_chunk_delivery() {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let (stream_reader, mut rx) =
            spawn_configured_reader(read_half, DeliveryGuarantee::BestEffort, 2.bytes(), 2);

        write_half.write_all(b"aabbcc").await.unwrap();
        write_half.flush().await.unwrap();
        sleep(Duration::from_millis(25)).await;

        for expected in [b"aa".as_slice(), b"bb".as_slice()] {
            match rx.recv().await.unwrap() {
                StreamEvent::Chunk(chunk) => {
                    assert_that!(chunk.as_ref()).is_equal_to(expected);
                }
                other => {
                    assert_that!(&other)
                        .fail(format_args!("expected buffered chunk, got {other:?}"));
                }
            }
        }

        write_half.write_all(b"dd").await.unwrap();
        write_half.flush().await.unwrap();
        drop(write_half);

        assert_that!(rx.recv().await.unwrap()).is_equal_to(StreamEvent::Gap);
        match rx.recv().await.unwrap() {
            StreamEvent::Chunk(chunk) => {
                assert_that!(chunk.as_ref()).is_equal_to(b"dd".as_slice());
            }
            other => {
                assert_that!(&other).fail(format_args!("expected resumed chunk, got {other:?}"));
            }
        }
        assert_that!(rx.recv().await.unwrap()).is_equal_to(StreamEvent::Eof);

        stream_reader.await.unwrap();
        assert_that!(rx.recv().await).is_none();
    }

    #[tokio::test]
    async fn configured_reader_publishes_read_error_without_panicking() {
        let (stream_reader, mut rx) = spawn_configured_reader(
            ReadErrorAfterBytes::new(b"ready\n", io::ErrorKind::BrokenPipe),
            DeliveryGuarantee::BestEffort,
            2.bytes(),
            64,
        );

        stream_reader.await.unwrap();

        let mut saw_error = false;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Chunk(_) | StreamEvent::Gap => {}
                StreamEvent::Eof => {
                    assert_that!(()).fail("read failure must not be reported as EOF");
                }
                StreamEvent::ReadError(err) => {
                    assert_that!(err.stream_name()).is_equal_to("custom");
                    assert_that!(err.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
                    saw_error = true;
                    break;
                }
            }
        }

        assert_that!(saw_error).is_true();
    }

    #[tokio::test]
    async fn reader_drains_after_consumer_drop() {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let stream = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            StreamConfig::builder()
                .reliable_for_active_subscribers()
                .no_replay()
                .read_chunk_size(16.bytes())
                .max_buffered_chunks(1)
                .build(),
        );

        let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
        drop(collector);
        wait_for_no_active_consumer(&stream).await;

        let idle_output = vec![b'x'; 4096];
        tokio::time::timeout(Duration::from_secs(1), write_half.write_all(&idle_output))
            .await
            .expect("reader should keep draining with no active consumer")
            .unwrap();
        sleep(Duration::from_millis(25)).await;

        let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
        write_half.write_all(b"tail").await.unwrap();
        drop(write_half);

        let bytes = collector.wait().await.unwrap();
        assert_that!(bytes.bytes).is_equal_to(b"tail".to_vec());
    }

    #[tokio::test]
    #[traced_test]
    async fn handles_backpressure_by_dropping_newer_chunks_after_channel_buffer_filled_up() {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            StreamConfig::builder()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(2)
                .build(),
        );

        let inspector = os.inspect_lines_async(
            |_line| async move {
                sleep(Duration::from_millis(100)).await;
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        let producer = tokio::spawn(async move {
            for count in 1..=15 {
                write_half
                    .write_all(format!("{count}\n").as_bytes())
                    .await
                    .unwrap();
                sleep(Duration::from_millis(25)).await;
            }
        });

        producer.await.unwrap();
        inspector.wait().await.unwrap();
        drop(os);

        logs_assert(|lines: &[&str]| {
            let lagged_logs = lines
                .iter()
                .filter(|line| line.contains("Stream reader is lagging behind lagged="))
                .count();
            if lagged_logs == 0 {
                return Err("Expected at least one lagged log".to_string());
            }
            Ok(())
        });
    }
}
