use crate::output_stream::OutputStream;
use crate::output_stream::subscription::EventSubscription;

pub(super) trait SubscribableOutputStream: OutputStream {
    fn subscribe_for_consumer(&self) -> impl EventSubscription;
}

macro_rules! impl_output_stream_consumer_api {
    (impl $($impl_header:tt)*) => {
        impl $($impl_header)* {
            /// Inspects chunks of output from the stream without storing them.
            ///
            /// The provided closure is called for each chunk of data. Return
            /// [`crate::Next::Continue`] to keep processing or [`crate::Next::Break`] to stop.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
            pub fn inspect_chunks(
                &self,
                f: impl FnMut($crate::Chunk) -> $crate::Next + Send + 'static,
            ) -> $crate::Inspector {
                $crate::output_stream::consumer::inspect::inspect_chunks(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self),
                    f,
                )
            }

            /// Inspects chunks of output from the stream without storing them, using an async closure.
            ///
            /// The provided async closure is called for each chunk of data. Return
            /// [`crate::Next::Continue`] to keep processing or [`crate::Next::Break`] to stop.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
            pub fn inspect_chunks_async<Fut>(
                &self,
                f: impl FnMut($crate::Chunk) -> Fut + Send + 'static,
            ) -> $crate::Inspector
            where
                Fut: ::std::future::Future<Output = $crate::Next> + Send,
            {
                $crate::output_stream::consumer::inspect::inspect_chunks_async(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self),
                    f,
                )
            }

            /// Inspects lines of output from the stream without storing them.
            ///
            /// The provided closure is called for each line. Return [`crate::Next::Continue`] to
            /// keep processing or [`crate::Next::Break`] to stop.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
            pub fn inspect_lines(
                &self,
                f: impl FnMut(::std::borrow::Cow<'_, str>) -> $crate::Next + Send + 'static,
                options: $crate::LineParsingOptions,
            ) -> $crate::Inspector {
                $crate::output_stream::consumer::inspect::inspect_lines(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self),
                    f,
                    options,
                )
            }

            /// Inspects lines of output from the stream without storing them, using an async closure.
            ///
            /// The provided async closure is called for each line. Return
            /// [`crate::Next::Continue`] to keep processing or [`crate::Next::Break`] to stop.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
            pub fn inspect_lines_async<Fut>(
                &self,
                f: impl FnMut(::std::borrow::Cow<'_, str>) -> Fut + Send + 'static,
                options: $crate::LineParsingOptions,
            ) -> $crate::Inspector
            where
                Fut: ::std::future::Future<Output = $crate::Next> + Send,
            {
                $crate::output_stream::consumer::inspect::inspect_lines_async(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self),
                    f,
                    options,
                )
            }

            /// Collects chunks from the stream into a sink.
            ///
            /// The provided closure is called for each chunk, with mutable access to the sink.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_chunks<S: $crate::Sink>(
                &self,
                into: S,
                collect: impl FnMut($crate::Chunk, &mut S) + Send + 'static,
            ) -> $crate::Collector<S> {
                $crate::output_stream::consumer::collect::collect_chunks(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self),
                    into,
                    collect,
                )
            }

            /// Collects chunks from the stream into a sink using an async collector.
            ///
            /// The provided async collector is called for each chunk, with mutable access to the sink.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_chunks_async<S, C>(&self, into: S, collect: C) -> $crate::Collector<S>
            where
                S: $crate::Sink,
                C: $crate::AsyncChunkCollector<S>,
            {
                $crate::output_stream::consumer::collect::collect_chunks_async(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self),
                    into,
                    collect,
                )
            }

            /// Collects lines from the stream into a sink.
            ///
            /// The provided closure is called for each line, with mutable access to the sink.
            /// Return [`crate::Next::Continue`] to keep processing or [`crate::Next::Break`] to stop.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_lines<S: $crate::Sink>(
                &self,
                into: S,
                collect: impl FnMut(::std::borrow::Cow<'_, str>, &mut S) -> $crate::Next
                    + Send
                    + 'static,
                options: $crate::LineParsingOptions,
            ) -> $crate::Collector<S> {
                $crate::output_stream::consumer::collect::collect_lines(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self),
                    into,
                    collect,
                    options,
                )
            }

            /// Collects lines from the stream into a sink using an async collector.
            ///
            /// The provided async collector is called for each line, with mutable access to the
            /// sink. Return [`crate::Next::Continue`] to keep processing or
            /// [`crate::Next::Break`] to stop.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_lines_async<S, C>(
                &self,
                into: S,
                collect: C,
                options: $crate::LineParsingOptions,
            ) -> $crate::Collector<S>
            where
                S: $crate::Sink,
                C: $crate::AsyncLineCollector<S>,
            {
                $crate::output_stream::consumer::collect::collect_lines_async(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self),
                    into,
                    collect,
                    options,
                )
            }

            /// Convenience method to collect chunks into a bounded byte vector.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_chunks_into_vec(
                &self,
                options: $crate::RawCollectionOptions,
            ) -> $crate::Collector<$crate::CollectedBytes> {
                self.collect_chunks($crate::CollectedBytes::new(), move |chunk, collected| {
                    collected.push_chunk(chunk.as_ref(), options);
                })
            }

            /// Trusted-output-only convenience method to collect all chunks into a `Vec<u8>`.
            ///
            /// This grows memory without a total output cap. Use it only when the child process and
            /// its output volume are trusted.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_all_chunks_into_vec_trusted(&self) -> $crate::Collector<Vec<u8>> {
                self.collect_chunks(Vec::new(), |chunk, vec| {
                    vec.extend_from_slice(chunk.as_ref());
                })
            }

            /// Convenience method to collect lines into a bounded line buffer.
            ///
            /// `parsing_options.max_line_length` must be non-zero. An unbounded single-line parser
            /// can allocate without limit before this collector receives any complete line.
            ///
            /// # Panics
            ///
            /// Panics if `parsing_options.max_line_length` is zero.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_lines_into_vec(
                &self,
                parsing_options: $crate::LineParsingOptions,
                collection_options: $crate::LineCollectionOptions,
            ) -> $crate::Collector<$crate::CollectedLines> {
                assert!(
                    parsing_options.max_line_length.bytes() > 0,
                    "parsing_options.max_line_length must be greater than zero for bounded line collection"
                );
                self.collect_lines(
                    $crate::CollectedLines::new(),
                    move |line, collected| {
                        collected.push_line(line.into_owned(), collection_options);
                        $crate::Next::Continue
                    },
                    parsing_options,
                )
            }

            /// Trusted-output-only convenience method to collect all lines into a `Vec<String>`.
            ///
            /// This grows memory without a total output cap. Use it only when the child process and
            /// its output volume are trusted. `LineParsingOptions::max_line_length` still controls
            /// the maximum size of one parsed line.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_all_lines_into_vec_trusted(
                &self,
                options: $crate::LineParsingOptions,
            ) -> $crate::Collector<Vec<String>> {
                self.collect_lines(
                    Vec::new(),
                    |line, vec| {
                        vec.push(line.into_owned());
                        $crate::Next::Continue
                    },
                    options,
                )
            }

            /// Collects chunks into an async writer.
            ///
            /// Sink write failures are handled according to `write_options`. Use
            /// [`crate::WriteCollectionOptions::fail_fast`] to stop collection and return
            /// [`crate::CollectorError::SinkWrite`] from [`crate::Collector::wait`] or
            /// [`crate::Collector::cancel`], [`crate::WriteCollectionOptions::log_and_continue`]
            /// to log each failure and keep collecting, or
            /// [`crate::WriteCollectionOptions::with_error_handler`] to make a per-error
            /// continue-or-stop decision.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_chunks_into_write<W, H>(
                &self,
                write: W,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> $crate::Collector<W>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                H: $crate::SinkWriteErrorHandler,
            {
                $crate::output_stream::consumer::write::collect_chunks_into_write(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self),
                    write,
                    write_options,
                )
            }

            /// Collects lines into an async writer.
            ///
            /// Parsed lines no longer include their trailing newline byte, so `mode` controls
            /// whether a `\n` delimiter should be reintroduced for each emitted line.
            ///
            /// Sink write failures are handled according to `write_options`. Use
            /// [`crate::WriteCollectionOptions::fail_fast`] to stop collection and return
            /// [`crate::CollectorError::SinkWrite`] from [`crate::Collector::wait`] or
            /// [`crate::Collector::cancel`], [`crate::WriteCollectionOptions::log_and_continue`]
            /// to log each failure and keep collecting, or
            /// [`crate::WriteCollectionOptions::with_error_handler`] to make a per-error
            /// continue-or-stop decision.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_lines_into_write<W, H>(
                &self,
                write: W,
                options: $crate::LineParsingOptions,
                mode: $crate::LineWriteMode,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> $crate::Collector<W>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                H: $crate::SinkWriteErrorHandler,
            {
                $crate::output_stream::consumer::write::collect_lines_into_write(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self),
                    write,
                    options,
                    mode,
                    write_options,
                )
            }

            /// Collects chunks into an async writer after mapping them with the provided function.
            ///
            /// Sink write failures are handled according to `write_options`. Use
            /// [`crate::WriteCollectionOptions::fail_fast`] to stop collection and return
            /// [`crate::CollectorError::SinkWrite`] from [`crate::Collector::wait`] or
            /// [`crate::Collector::cancel`], [`crate::WriteCollectionOptions::log_and_continue`]
            /// to log each failure and keep collecting, or
            /// [`crate::WriteCollectionOptions::with_error_handler`] to make a per-error
            /// continue-or-stop decision.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_chunks_into_write_mapped<W, B, H>(
                &self,
                write: W,
                mapper: impl Fn($crate::Chunk) -> B + Send + Sync + Copy + 'static,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> $crate::Collector<W>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                B: AsRef<[u8]> + Send,
                H: $crate::SinkWriteErrorHandler,
            {
                $crate::output_stream::consumer::write::collect_chunks_into_write_mapped(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self),
                    write,
                    mapper,
                    write_options,
                )
            }

            /// Collects lines into an async writer after mapping them with the provided function.
            ///
            /// `mode` applies after `mapper`: choose [`crate::LineWriteMode::AsIs`] when the
            /// mapped output already contains delimiters, or [`crate::LineWriteMode::AppendLf`]
            /// to append `\n` after each mapped line.
            ///
            /// Sink write failures are handled according to `write_options`. Use
            /// [`crate::WriteCollectionOptions::fail_fast`] to stop collection and return
            /// [`crate::CollectorError::SinkWrite`] from [`crate::Collector::wait`] or
            /// [`crate::Collector::cancel`], [`crate::WriteCollectionOptions::log_and_continue`]
            /// to log each failure and keep collecting, or
            /// [`crate::WriteCollectionOptions::with_error_handler`] to make a per-error
            /// continue-or-stop decision.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_lines_into_write_mapped<W, B, H>(
                &self,
                write: W,
                mapper: impl Fn(::std::borrow::Cow<'_, str>) -> B + Send + Sync + Copy + 'static,
                options: $crate::LineParsingOptions,
                mode: $crate::LineWriteMode,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> $crate::Collector<W>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                B: AsRef<[u8]> + Send,
                H: $crate::SinkWriteErrorHandler,
            {
                $crate::output_stream::consumer::write::collect_lines_into_write_mapped(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self),
                    write,
                    mapper,
                    options,
                    mode,
                    write_options,
                )
            }

            /// Waits for a line that matches the given predicate.
            ///
            /// Returns `Ok(`[`crate::WaitForLineResult::Matched`]`)` if a matching line is found,
            /// or `Ok(`[`crate::WaitForLineResult::StreamClosed`]`)` if the stream ends first.
            /// This method never returns [`crate::WaitForLineResult::Timeout`]; use
            /// [`Self::wait_for_line_with_timeout`] if you need a bounded wait.
            ///
            /// The waiter starts at the earliest output currently available to new consumers. With
            /// replay enabled and unsealed, that can include retained past output; otherwise it
            /// starts at live output. Backends apply their configured consumer limits; for example,
            /// single-subscriber streams allow only one active inspector, collector, or line waiter
            /// at a time.
            ///
            /// When chunks are dropped in [`crate::DeliveryGuarantee::BestEffort`] mode, this
            /// waiter discards any partial line in progress and resynchronizes at the next newline
            /// instead of matching across the gap.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamReadError`] if the underlying stream fails while being read.
            #[must_use]
            pub fn wait_for_line(
                &self,
                predicate: impl Fn(::std::borrow::Cow<'_, str>) -> bool + Send + Sync + 'static,
                options: $crate::LineParsingOptions,
            ) -> $crate::output_stream::line_waiter::LineWaiter {
                let subscription = <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self);
                $crate::output_stream::line_waiter::LineWaiter::new(
                    $crate::output_stream::consumer::wait::wait_for_line_with_optional_timeout(
                        subscription,
                        predicate,
                        options,
                        None,
                    ),
                )
            }

            /// Waits for a line that matches the given predicate, with a timeout.
            ///
            /// Returns `Ok(`[`crate::WaitForLineResult::Matched`]`)` if a matching line is found,
            /// `Ok(`[`crate::WaitForLineResult::StreamClosed`]`)` if the stream ends first, or
            /// `Ok(`[`crate::WaitForLineResult::Timeout`]`)` if the timeout expires first.
            ///
            /// The waiter starts at the earliest output currently available to new consumers. With
            /// replay enabled and unsealed, that can include retained past output; otherwise it
            /// starts at live output. Backends apply their configured consumer limits; for example,
            /// single-subscriber streams allow only one active inspector, collector, or line waiter
            /// at a time.
            ///
            /// When chunks are dropped in [`crate::DeliveryGuarantee::BestEffort`] mode, this
            /// waiter discards any partial line in progress and resynchronizes at the next newline
            /// instead of matching across the gap.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamReadError`] if the underlying stream fails while being read.
            #[must_use]
            pub fn wait_for_line_with_timeout(
                &self,
                predicate: impl Fn(::std::borrow::Cow<'_, str>) -> bool + Send + Sync + 'static,
                options: $crate::LineParsingOptions,
                timeout: ::std::time::Duration,
            ) -> $crate::output_stream::line_waiter::LineWaiter {
                let subscription = <Self as $crate::output_stream::backend::consumer_api::SubscribableOutputStream>::subscribe_for_consumer(self);
                $crate::output_stream::line_waiter::LineWaiter::new(
                    $crate::output_stream::consumer::wait::wait_for_line_with_optional_timeout(
                        subscription,
                        predicate,
                        options,
                        Some(timeout),
                    ),
                )
            }
        }
    };
}

pub(super) use impl_output_stream_consumer_api;
