use super::super::reader::{read_chunked_best_effort, read_chunked_reliable};
use super::super::state::{ActiveSubscriber, ConfiguredShared};
use crate::NumBytes;
use crate::output_stream::event::StreamEvent;
use crate::output_stream::policy::DeliveryGuarantee;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::sync::mpsc;

pub(super) fn spawn_configured_reader<R>(
    read: R,
    delivery_guarantee: DeliveryGuarantee,
    read_chunk_size: NumBytes,
    max_buffered_chunks: usize,
) -> (
    tokio::task::JoinHandle<()>,
    mpsc::Receiver<StreamEvent>,
    Arc<ConfiguredShared>,
)
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

    let stream_reader = match delivery_guarantee {
        DeliveryGuarantee::BestEffort => tokio::spawn(read_chunked_best_effort(
            read,
            Arc::clone(&shared),
            active_rx,
            read_chunk_size,
            None,
            "custom",
        )),
        DeliveryGuarantee::ReliableForActiveSubscribers => tokio::spawn(read_chunked_reliable(
            read,
            Arc::clone(&shared),
            active_rx,
            read_chunk_size,
            None,
            "custom",
        )),
    };

    (stream_reader, receiver, shared)
}

pub(super) async fn recv_event_with_timeout(rx: &mut mpsc::Receiver<StreamEvent>) -> StreamEvent {
    tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("timed out waiting for stream event")
        .expect("stream closed before expected event")
}
