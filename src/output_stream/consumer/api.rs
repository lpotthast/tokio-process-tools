macro_rules! impl_output_stream_consumer_api {
    (impl $($impl_header:tt)*) => {
        #[allow(dead_code)]
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
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
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
                Fut: ::std::future::Future<Output = $crate::Next> + Send + 'static,
            {
                $crate::output_stream::consumer::inspect::inspect_chunks_async(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
                    f,
                )
            }

            /// Inspects lines of output from the stream without storing them.
            ///
            /// The provided closure is called for each line. Return [`crate::Next::Continue`] to
            /// keep processing or [`crate::Next::Break`] to stop.
            ///
            /// # Panics
            ///
            /// Panics if `options.max_line_length` is zero.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
            pub fn inspect_lines(
                &self,
                f: impl FnMut(::std::borrow::Cow<'_, str>) -> $crate::Next + Send + 'static,
                options: $crate::LineParsingOptions,
            ) -> $crate::Inspector {
                $crate::output_stream::consumer::inspect::inspect_lines(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
                    f,
                    options,
                )
            }

            /// Inspects lines of output from the stream without storing them, using an async closure.
            ///
            /// The provided async closure is called for each line. Return
            /// [`crate::Next::Continue`] to keep processing or [`crate::Next::Break`] to stop.
            ///
            /// # Panics
            ///
            /// Panics if `options.max_line_length` is zero.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
            pub fn inspect_lines_async<Fut>(
                &self,
                f: impl FnMut(::std::borrow::Cow<'_, str>) -> Fut + Send + 'static,
                options: $crate::LineParsingOptions,
            ) -> $crate::Inspector
            where
                Fut: ::std::future::Future<Output = $crate::Next> + Send + 'static,
            {
                $crate::output_stream::consumer::inspect::inspect_lines_async(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
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
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
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
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
                    into,
                    collect,
                )
            }

            /// Collects lines from the stream into a sink.
            ///
            /// The provided closure is called for each line, with mutable access to the sink.
            /// Return [`crate::Next::Continue`] to keep processing or [`crate::Next::Break`] to stop.
            ///
            /// # Panics
            ///
            /// Panics if `options.max_line_length` is zero.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_lines<S: $crate::Sink>(
                &self,
                into: S,
                collect: impl FnMut(::std::borrow::Cow<'_, str>, &mut S) -> $crate::Next
                    + Send
                    + 'static,
                options: $crate::LineParsingOptions,
            ) -> $crate::Collector<S> {
                assert!(
                    options.max_line_length.bytes() > 0,
                    "LineParsingOptions::max_line_length must be greater than zero"
                );
                $crate::output_stream::consumer::collect::collect_lines(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
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
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
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
                $crate::output_stream::consumer::collect::collect_chunks_into_vec(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
                    options,
                )
            }

            /// Convenience method to collect lines into a line buffer.
            ///
            /// `parsing_options.max_line_length` must be non-zero unless
            /// `collection_options` is [`crate::LineCollectionOptions::TrustedUnbounded`].
            ///
            /// # Panics
            ///
            /// Panics if `parsing_options.max_line_length` is zero and bounded collection is used.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_lines_into_vec(
                &self,
                parsing_options: $crate::LineParsingOptions,
                collection_options: $crate::LineCollectionOptions,
            ) -> $crate::Collector<$crate::CollectedLines> {
                $crate::output_stream::consumer::collect::collect_lines_into_vec(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
                    parsing_options,
                    collection_options,
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
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
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
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
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
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
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
                    <Self as $crate::output_stream::Subscribable>::subscribe(self),
                    write,
                    mapper,
                    options,
                    mode,
                    write_options,
                )
            }

            /// Waits for a line that matches the given predicate within `timeout`.
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
            ///
            /// # Panics
            ///
            /// Panics if `options.max_line_length` is zero.
            #[must_use]
            pub fn wait_for_line(
                &self,
                timeout: ::std::time::Duration,
                predicate: impl Fn(::std::borrow::Cow<'_, str>) -> bool + Send + Sync + 'static,
                options: $crate::LineParsingOptions,
            ) -> $crate::output_stream::consumer::line_waiter::LineWaiter {
                let subscription = <Self as $crate::output_stream::Subscribable>::subscribe(self);
                $crate::output_stream::consumer::line_waiter::LineWaiter::new(
                    $crate::output_stream::consumer::wait::wait_for_line_bounded(
                        subscription,
                        predicate,
                        options,
                        timeout,
                    ),
                )
            }
        }
    };
}

macro_rules! impl_fallible_output_stream_consumer_api {
    (impl $($impl_header:tt)*) => {
        #[allow(dead_code)]
        impl $($impl_header)* {
            /// Tries to inspect chunks of output from the stream without storing them.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn inspect_chunks(
                &self,
                f: impl FnMut($crate::Chunk) -> $crate::Next + Send + 'static,
            ) -> Result<$crate::Inspector, $crate::StreamConsumerError> {
                Ok($crate::output_stream::consumer::inspect::inspect_chunks(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    f,
                ))
            }

            /// Tries to inspect chunks of output from the stream without storing them, using an async closure.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn inspect_chunks_async<Fut>(
                &self,
                f: impl FnMut($crate::Chunk) -> Fut + Send + 'static,
            ) -> Result<$crate::Inspector, $crate::StreamConsumerError>
            where
                Fut: ::std::future::Future<Output = $crate::Next> + Send + 'static,
            {
                Ok($crate::output_stream::consumer::inspect::inspect_chunks_async(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    f,
                ))
            }

            /// Tries to inspect lines of output from the stream without storing them.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            ///
            /// # Panics
            ///
            /// Panics if `options.max_line_length` is zero.
            pub fn inspect_lines(
                &self,
                f: impl FnMut(::std::borrow::Cow<'_, str>) -> $crate::Next + Send + 'static,
                options: $crate::LineParsingOptions,
            ) -> Result<$crate::Inspector, $crate::StreamConsumerError> {
                Ok($crate::output_stream::consumer::inspect::inspect_lines(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    f,
                    options,
                ))
            }

            /// Tries to inspect lines of output from the stream without storing them, using an async closure.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            ///
            /// # Panics
            ///
            /// Panics if `options.max_line_length` is zero.
            pub fn inspect_lines_async<Fut>(
                &self,
                f: impl FnMut(::std::borrow::Cow<'_, str>) -> Fut + Send + 'static,
                options: $crate::LineParsingOptions,
            ) -> Result<$crate::Inspector, $crate::StreamConsumerError>
            where
                Fut: ::std::future::Future<Output = $crate::Next> + Send + 'static,
            {
                Ok($crate::output_stream::consumer::inspect::inspect_lines_async(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    f,
                    options,
                ))
            }

            /// Tries to collect chunks from the stream into a sink.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn collect_chunks<S: $crate::Sink>(
                &self,
                into: S,
                collect: impl FnMut($crate::Chunk, &mut S) + Send + 'static,
            ) -> Result<$crate::Collector<S>, $crate::StreamConsumerError> {
                Ok($crate::output_stream::consumer::collect::collect_chunks(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    into,
                    collect,
                ))
            }

            /// Tries to collect chunks from the stream into a sink using an async collector.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn collect_chunks_async<S, C>(
                &self,
                into: S,
                collect: C,
            ) -> Result<$crate::Collector<S>, $crate::StreamConsumerError>
            where
                S: $crate::Sink,
                C: $crate::AsyncChunkCollector<S>,
            {
                Ok($crate::output_stream::consumer::collect::collect_chunks_async(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    into,
                    collect,
                ))
            }

            /// Tries to collect lines from the stream into a sink.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            ///
            /// # Panics
            ///
            /// Panics if `options.max_line_length` is zero.
            pub fn collect_lines<S: $crate::Sink>(
                &self,
                into: S,
                collect: impl FnMut(::std::borrow::Cow<'_, str>, &mut S) -> $crate::Next
                    + Send
                    + 'static,
                options: $crate::LineParsingOptions,
            ) -> Result<$crate::Collector<S>, $crate::StreamConsumerError> {
                assert!(
                    options.max_line_length.bytes() > 0,
                    "LineParsingOptions::max_line_length must be greater than zero"
                );
                Ok($crate::output_stream::consumer::collect::collect_lines(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    into,
                    collect,
                    options,
                ))
            }

            /// Tries to collect lines from the stream into a sink using an async collector.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn collect_lines_async<S, C>(
                &self,
                into: S,
                collect: C,
                options: $crate::LineParsingOptions,
            ) -> Result<$crate::Collector<S>, $crate::StreamConsumerError>
            where
                S: $crate::Sink,
                C: $crate::AsyncLineCollector<S>,
            {
                Ok($crate::output_stream::consumer::collect::collect_lines_async(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    into,
                    collect,
                    options,
                ))
            }

            /// Tries to collect chunks into a bounded byte vector.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn collect_chunks_into_vec(
                &self,
                options: $crate::RawCollectionOptions,
            ) -> Result<$crate::Collector<$crate::CollectedBytes>, $crate::StreamConsumerError> {
                Ok($crate::output_stream::consumer::collect::collect_chunks_into_vec(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    options,
                ))
            }

            /// Tries to collect lines into a line buffer.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            ///
            /// # Panics
            ///
            /// Panics if `parsing_options.max_line_length` is zero and bounded collection is used.
            pub fn collect_lines_into_vec(
                &self,
                parsing_options: $crate::LineParsingOptions,
                collection_options: $crate::LineCollectionOptions,
            ) -> Result<$crate::Collector<$crate::CollectedLines>, $crate::StreamConsumerError> {
                Ok($crate::output_stream::consumer::collect::collect_lines_into_vec(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    parsing_options,
                    collection_options,
                ))
            }

            /// Tries to collect chunks into an async writer.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn collect_chunks_into_write<W, H>(
                &self,
                write: W,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> Result<$crate::Collector<W>, $crate::StreamConsumerError>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                H: $crate::SinkWriteErrorHandler,
            {
                Ok($crate::output_stream::consumer::write::collect_chunks_into_write(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    write,
                    write_options,
                ))
            }

            /// Tries to collect lines into an async writer.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn collect_lines_into_write<W, H>(
                &self,
                write: W,
                options: $crate::LineParsingOptions,
                mode: $crate::LineWriteMode,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> Result<$crate::Collector<W>, $crate::StreamConsumerError>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                H: $crate::SinkWriteErrorHandler,
            {
                Ok($crate::output_stream::consumer::write::collect_lines_into_write(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    write,
                    options,
                    mode,
                    write_options,
                ))
            }

            /// Tries to collect chunks into an async writer after mapping them.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn collect_chunks_into_write_mapped<W, B, H>(
                &self,
                write: W,
                mapper: impl Fn($crate::Chunk) -> B + Send + Sync + Copy + 'static,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> Result<$crate::Collector<W>, $crate::StreamConsumerError>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                B: AsRef<[u8]> + Send,
                H: $crate::SinkWriteErrorHandler,
            {
                Ok($crate::output_stream::consumer::write::collect_chunks_into_write_mapped(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    write,
                    mapper,
                    write_options,
                ))
            }

            /// Tries to collect lines into an async writer after mapping them.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn collect_lines_into_write_mapped<W, B, H>(
                &self,
                write: W,
                mapper: impl Fn(::std::borrow::Cow<'_, str>) -> B + Send + Sync + Copy + 'static,
                options: $crate::LineParsingOptions,
                mode: $crate::LineWriteMode,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> Result<$crate::Collector<W>, $crate::StreamConsumerError>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                B: AsRef<[u8]> + Send,
                H: $crate::SinkWriteErrorHandler,
            {
                Ok($crate::output_stream::consumer::write::collect_lines_into_write_mapped(
                    <Self as $crate::output_stream::OutputStream>::name(self),
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?,
                    write,
                    mapper,
                    options,
                    mode,
                    write_options,
                ))
            }

            /// Tries to wait for a line that matches the given predicate within `timeout`.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the line waiter.
            ///
            /// # Panics
            ///
            /// Panics if `options.max_line_length` is zero.
            pub fn wait_for_line(
                &self,
                timeout: ::std::time::Duration,
                predicate: impl Fn(::std::borrow::Cow<'_, str>) -> bool + Send + Sync + 'static,
                options: $crate::LineParsingOptions,
            ) -> Result<$crate::output_stream::consumer::line_waiter::LineWaiter, $crate::StreamConsumerError> {
                let subscription =
                    <Self as $crate::output_stream::TrySubscribable>::try_subscribe(self)?;
                Ok($crate::output_stream::consumer::line_waiter::LineWaiter::new(
                    $crate::output_stream::consumer::wait::wait_for_line_bounded(
                        subscription,
                        predicate,
                        options,
                        timeout,
                    ),
                ))
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use crate::output_stream::event::{Chunk, StreamEvent};
    use crate::output_stream::num_bytes::NumBytes;
    use crate::output_stream::{OutputStream, Subscribable, Subscription};
    use crate::{
        CollectionOverflowBehavior, LineCollectionOptions, LineParsingOptions, Next, NumBytesExt,
        WaitForLineResult,
    };
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::cell::Cell;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::mpsc;

    struct TestOutputStream {
        receiver: Mutex<Option<mpsc::Receiver<StreamEvent>>>,
    }

    impl TestOutputStream {
        fn new(receiver: mpsc::Receiver<StreamEvent>) -> Self {
            Self {
                receiver: Mutex::new(Some(receiver)),
            }
        }
    }

    impl OutputStream for TestOutputStream {
        fn read_chunk_size(&self) -> NumBytes {
            4.bytes()
        }

        fn max_buffered_chunks(&self) -> usize {
            4
        }

        fn name(&self) -> &'static str {
            "custom"
        }
    }

    impl Subscribable for TestOutputStream {
        fn subscribe(&self) -> impl Subscription {
            self.receiver
                .lock()
                .expect("test receiver mutex poisoned")
                .take()
                .expect("test stream supports one subscription")
        }
    }

    impl_output_stream_consumer_api! {
        impl TestOutputStream
    }

    fn stream_with_sender() -> (TestOutputStream, mpsc::Sender<StreamEvent>) {
        let (tx, rx) = mpsc::channel(4);
        (TestOutputStream::new(rx), tx)
    }

    #[tokio::test]
    async fn wait_for_line_subscribes_before_polling() {
        let (stream, tx) = stream_with_sender();

        let waiter = stream.wait_for_line(
            Duration::from_secs(1),
            |line| line == "ready",
            LineParsingOptions::default(),
        );
        tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(b"ready\n"))))
            .await
            .unwrap();
        tx.send(StreamEvent::Eof).await.unwrap();

        assert_that!(waiter.await).is_equal_to(Ok(WaitForLineResult::Matched));
    }

    #[tokio::test]
    async fn wait_for_line_subscribes_before_polling_with_timeout() {
        let (stream, tx) = stream_with_sender();

        let waiter = stream.wait_for_line(
            Duration::from_secs(1),
            |line| line == "ready",
            LineParsingOptions::default(),
        );
        tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(b"ready\n"))))
            .await
            .unwrap();
        tx.send(StreamEvent::Eof).await.unwrap();

        assert_that!(waiter.await).is_equal_to(Ok(WaitForLineResult::Matched));
    }

    #[derive(Default)]
    struct SendOnlyLineSink {
        lines: Vec<String>,
        line_count: Cell<usize>,
    }

    #[tokio::test]
    async fn collect_lines_accepts_send_only_sink() {
        let (stream, tx) = stream_with_sender();
        let collector = stream.collect_lines(
            SendOnlyLineSink::default(),
            |line, sink| {
                sink.lines.push(line.into_owned());
                sink.line_count.set(sink.line_count.get() + 1);
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(
            b"alpha\nbeta",
        ))))
        .await
        .unwrap();
        tx.send(StreamEvent::Eof).await.unwrap();

        let sink = collector.wait().await.unwrap();
        assert_that!(sink.lines).is_equal_to(vec!["alpha".to_string(), "beta".to_string()]);
        assert_that!(sink.line_count.get()).is_equal_to(2);
    }

    #[tokio::test]
    async fn bounded_line_collection_drains_until_eof_after_limit_is_reached() {
        let (stream, tx) = stream_with_sender();
        let collector = stream.collect_lines_into_vec(
            LineParsingOptions::default(),
            LineCollectionOptions::Bounded {
                max_bytes: 3.bytes(),
                max_lines: 1,
                overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
            },
        );

        tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(
            b"one\ntwo\nthree\n",
        ))))
        .await
        .unwrap();
        tx.send(StreamEvent::Eof).await.unwrap();

        let collected = collector.wait().await.unwrap();
        assert_that!(collected.lines().iter().map(String::as_str)).contains_exactly(["one"]);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    #[should_panic(expected = "LineParsingOptions::max_line_length must be greater than zero")]
    fn collect_lines_rejects_zero_max_line_length() {
        let (stream, _tx) = stream_with_sender();
        let _collector = stream.collect_lines(
            Vec::<String>::new(),
            |line, sink| {
                sink.push(line.into_owned());
                Next::Continue
            },
            LineParsingOptions::builder()
                .max_line_length(0.bytes())
                .overflow_behavior(crate::LineOverflowBehavior::default())
                .build(),
        );
    }

    #[test]
    #[should_panic(
        expected = "parsing_options.max_line_length must be greater than zero unless line collection is trusted-unbounded"
    )]
    fn bounded_line_collection_rejects_unbounded_line_parser() {
        let (stream, _tx) = stream_with_sender();
        let _collector = stream.collect_lines_into_vec(
            LineParsingOptions::builder()
                .max_line_length(0.bytes())
                .overflow_behavior(crate::LineOverflowBehavior::default())
                .build(),
            LineCollectionOptions::Bounded {
                max_bytes: 3.bytes(),
                max_lines: 1,
                overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
            },
        );
    }

    #[tokio::test]
    async fn trusted_unbounded_line_collection_allows_unbounded_line_parser() {
        let (stream, tx) = stream_with_sender();
        let collector = stream.collect_lines_into_vec(
            LineParsingOptions::builder()
                .max_line_length(0.bytes())
                .overflow_behavior(crate::LineOverflowBehavior::default())
                .build(),
            LineCollectionOptions::TrustedUnbounded,
        );

        tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(
            b"unterminated long line",
        ))))
        .await
        .unwrap();
        tx.send(StreamEvent::Eof).await.unwrap();

        let collected = collector.wait().await.unwrap();
        assert_that!(collected.lines().iter().map(String::as_str))
            .contains_exactly(["unterminated long line"]);
        assert_that!(collected.truncated()).is_false();
    }
}
