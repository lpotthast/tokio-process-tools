#![cfg(test)]

use crate::output_stream::event::StreamEvent;
use tokio::sync::mpsc;

pub(super) async fn event_receiver(events: Vec<StreamEvent>) -> mpsc::Receiver<StreamEvent> {
    let (tx, rx) = mpsc::channel(events.len().max(1));
    for event in events {
        tx.send(event).await.unwrap();
    }
    drop(tx);
    rx
}
