use crate::output_stream::StreamEvent;
use std::future::Future;

pub(crate) trait EventSubscription: Send + 'static {
    fn next_event(&mut self) -> impl Future<Output = Option<StreamEvent>> + Send + '_;
}
