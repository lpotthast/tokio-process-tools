use crate::collector::Collector;
use crate::output_stream::OutputStream;
use crate::output_stream::backend::broadcast::BroadcastOutputStream;
use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
use crate::output_stream::collection::{CollectedBytes, CollectedLines};
use crate::output_stream::collection::{LineCollectionOptions, RawCollectionOptions};
use crate::output_stream::line::LineParsingOptions;
use crate::output_stream::policy::{Delivery, Replay};

pub trait CollectableOutputStream: OutputStream {
    fn collect_lines_into_vec(
        &self,
        parsing_options: LineParsingOptions,
        collection_options: LineCollectionOptions,
    ) -> Collector<CollectedLines>;

    fn collect_all_lines_into_vec_trusted(
        &self,
        options: LineParsingOptions,
    ) -> Collector<Vec<String>>;

    fn collect_chunks_into_vec(&self, options: RawCollectionOptions) -> Collector<CollectedBytes>;

    fn collect_all_chunks_into_vec_trusted(&self) -> Collector<Vec<u8>>;
}

impl<D, R> CollectableOutputStream for BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn collect_lines_into_vec(
        &self,
        parsing_options: LineParsingOptions,
        collection_options: LineCollectionOptions,
    ) -> Collector<CollectedLines> {
        BroadcastOutputStream::collect_lines_into_vec(self, parsing_options, collection_options)
    }

    fn collect_all_lines_into_vec_trusted(
        &self,
        options: LineParsingOptions,
    ) -> Collector<Vec<String>> {
        BroadcastOutputStream::collect_all_lines_into_vec_trusted(self, options)
    }

    fn collect_chunks_into_vec(&self, options: RawCollectionOptions) -> Collector<CollectedBytes> {
        BroadcastOutputStream::collect_chunks_into_vec(self, options)
    }

    fn collect_all_chunks_into_vec_trusted(&self) -> Collector<Vec<u8>> {
        BroadcastOutputStream::collect_all_chunks_into_vec_trusted(self)
    }
}

impl CollectableOutputStream for SingleSubscriberOutputStream {
    fn collect_lines_into_vec(
        &self,
        parsing_options: LineParsingOptions,
        collection_options: LineCollectionOptions,
    ) -> Collector<CollectedLines> {
        SingleSubscriberOutputStream::collect_lines_into_vec(
            self,
            parsing_options,
            collection_options,
        )
    }

    fn collect_all_lines_into_vec_trusted(
        &self,
        options: LineParsingOptions,
    ) -> Collector<Vec<String>> {
        SingleSubscriberOutputStream::collect_all_lines_into_vec_trusted(self, options)
    }

    fn collect_chunks_into_vec(&self, options: RawCollectionOptions) -> Collector<CollectedBytes> {
        SingleSubscriberOutputStream::collect_chunks_into_vec(self, options)
    }

    fn collect_all_chunks_into_vec_trusted(&self) -> Collector<Vec<u8>> {
        SingleSubscriberOutputStream::collect_all_chunks_into_vec_trusted(self)
    }
}
