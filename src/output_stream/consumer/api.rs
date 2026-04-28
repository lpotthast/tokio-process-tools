/// Internal helper that emits a consumer-method body. Both the infallible and the fallible
/// top-level macros expand to calls of the form
/// `consumer_fn(stream_name, subscription, ...)`; this helper hides that boilerplate.
///
/// `Direct` mode produces an unwrapped expression for the infallible API; `Fallible` mode
/// uses `try_subscribe()?` and wraps the result in `Ok(...)` for the result-returning API.
/// Pass `self` explicitly because macro hygiene hides the surrounding method's `self` binding.
macro_rules! __consumer_call {
    (Direct, $self:ident, $fn:path $(, $arg:expr)* $(,)?) => {
        $fn(
            <Self as $crate::output_stream::OutputStream>::name($self),
            <Self as $crate::output_stream::Subscribable>::subscribe($self)
            $(, $arg)*
        )
    };
    (Fallible, $self:ident, $fn:path $(, $arg:expr)* $(,)?) => {
        Ok($fn(
            <Self as $crate::output_stream::OutputStream>::name($self),
            <Self as $crate::output_stream::TrySubscribable>::try_subscribe($self)?
            $(, $arg)*
        ))
    };
}

macro_rules! impl_output_stream_consumer_api {
    (impl $($impl_header:tt)*) => {
        #[allow(dead_code)]
        impl $($impl_header)* {
            /// Drives the provided synchronous [`StreamVisitor`](crate::StreamVisitor) over this
            /// stream and returns a [`Consumer`](crate::Consumer) that owns the spawned task.
            ///
            /// All built-in `inspect_*`, `collect_*`, and `wait_for_line` factories construct a
            /// built-in visitor and call this method internally; reach for `consume_with` when
            /// the closure-shaped factories don't fit and you need direct access to the
            /// chunk/gap/EOF lifecycle. The returned [`Consumer`](crate::Consumer)'s
            /// [`wait`](crate::Consumer::wait) yields whatever the visitor produces from
            /// [`StreamVisitor::into_output`](crate::StreamVisitor::into_output).
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your visitor is never invoked and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
            pub fn consume_with<V>(&self, visitor: V) -> $crate::Consumer<V::Output>
            where
                V: $crate::StreamVisitor,
            {
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::consumer::consume_with,
                    visitor,
                )
            }

            /// Drives the provided asynchronous [`AsyncStreamVisitor`](crate::AsyncStreamVisitor)
            /// over this stream and returns a [`Consumer`](crate::Consumer) that owns the spawned
            /// task.
            ///
            /// Use this when observing a chunk requires `.await` (for example, forwarding chunks
            /// to an async writer or channel). See [`consume_with`](Self::consume_with) for the
            /// synchronous variant.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your visitor is never invoked and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
            pub fn consume_with_async<V>(&self, visitor: V) -> $crate::Consumer<V::Output>
            where
                V: $crate::AsyncStreamVisitor,
            {
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::consumer::consume_with_async,
                    visitor,
                )
            }

            /// Inspects chunks of output from the stream without storing them.
            ///
            /// The provided closure is called for each chunk of data. Return
            /// [`crate::Next::Continue`] to keep processing or [`crate::Next::Break`] to stop.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
            pub fn inspect_chunks(
                &self,
                f: impl FnMut($crate::Chunk) -> $crate::Next + Send + 'static,
            ) -> $crate::Inspector {
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::inspect::inspect_chunks,
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
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::inspect::inspect_chunks_async,
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
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::inspect::inspect_lines,
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
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::inspect::inspect_lines_async,
                    f,
                    options,
                )
            }

            /// Collects chunks from the stream into a sink.
            ///
            /// The provided closure is called for each chunk, with mutable access to the sink.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_chunks<S: $crate::Sink>(
                &self,
                into: S,
                collect: impl FnMut($crate::Chunk, &mut S) + Send + 'static,
            ) -> $crate::Consumer<S> {
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::collect::collect_chunks,
                    into,
                    collect,
                )
            }

            /// Collects chunks from the stream into a sink using an async collector.
            ///
            /// The provided async collector is called for each chunk, with mutable access to the sink.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_chunks_async<S, C>(&self, into: S, collect: C) -> $crate::Consumer<S>
            where
                S: $crate::Sink,
                C: $crate::AsyncChunkCollector<S>,
            {
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::collect::collect_chunks_async,
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
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_lines<S: $crate::Sink>(
                &self,
                into: S,
                collect: impl FnMut(::std::borrow::Cow<'_, str>, &mut S) -> $crate::Next
                    + Send
                    + 'static,
                options: $crate::LineParsingOptions,
            ) -> $crate::Consumer<S> {
                assert!(
                    options.max_line_length.bytes() > 0,
                    "LineParsingOptions::max_line_length must be greater than zero"
                );
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::collect::collect_lines,
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
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_lines_async<S, C>(
                &self,
                into: S,
                collect: C,
                options: $crate::LineParsingOptions,
            ) -> $crate::Consumer<S>
            where
                S: $crate::Sink,
                C: $crate::AsyncLineCollector<S>,
            {
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::collect::collect_lines_async,
                    into,
                    collect,
                    options,
                )
            }

            /// Convenience method to collect chunks into a bounded byte vector.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_chunks_into_vec(
                &self,
                options: $crate::RawCollectionOptions,
            ) -> $crate::Consumer<$crate::CollectedBytes> {
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::collect::collect_chunks_into_vec,
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
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_lines_into_vec(
                &self,
                parsing_options: $crate::LineParsingOptions,
                collection_options: $crate::LineCollectionOptions,
            ) -> $crate::Consumer<$crate::CollectedLines> {
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::collect::collect_lines_into_vec,
                    parsing_options,
                    collection_options,
                )
            }

            /// Collects chunks into an async writer.
            ///
            /// Sink write failures are handled according to `write_options`. Use
            /// [`crate::WriteCollectionOptions::fail_fast`] to stop collection and return
            /// [`crate::ConsumerError::SinkWrite`] from [`crate::Consumer::wait`] or
            /// [`crate::Consumer::cancel`], [`crate::WriteCollectionOptions::log_and_continue`]
            /// to log each failure and keep collecting, or
            /// [`crate::WriteCollectionOptions::with_error_handler`] to make a per-error
            /// continue-or-stop decision.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_chunks_into_write<W, H>(
                &self,
                write: W,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> $crate::Consumer<W>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                H: $crate::SinkWriteErrorHandler,
            {
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::write::collect_chunks_into_write,
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
            /// [`crate::ConsumerError::SinkWrite`] from [`crate::Consumer::wait`] or
            /// [`crate::Consumer::cancel`], [`crate::WriteCollectionOptions::log_and_continue`]
            /// to log each failure and keep collecting, or
            /// [`crate::WriteCollectionOptions::with_error_handler`] to make a per-error
            /// continue-or-stop decision.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_lines_into_write<W, H>(
                &self,
                write: W,
                options: $crate::LineParsingOptions,
                mode: $crate::LineWriteMode,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> $crate::Consumer<W>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                H: $crate::SinkWriteErrorHandler,
            {
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::write::collect_lines_into_write,
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
            /// [`crate::ConsumerError::SinkWrite`] from [`crate::Consumer::wait`] or
            /// [`crate::Consumer::cancel`], [`crate::WriteCollectionOptions::log_and_continue`]
            /// to log each failure and keep collecting, or
            /// [`crate::WriteCollectionOptions::with_error_handler`] to make a per-error
            /// continue-or-stop decision.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_chunks_into_write_mapped<W, B, H>(
                &self,
                write: W,
                mapper: impl Fn($crate::Chunk) -> B + Send + Sync + 'static,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> $crate::Consumer<W>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                B: AsRef<[u8]> + Send + 'static,
                H: $crate::SinkWriteErrorHandler,
            {
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::write::collect_chunks_into_write_mapped,
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
            /// [`crate::ConsumerError::SinkWrite`] from [`crate::Consumer::wait`] or
            /// [`crate::Consumer::cancel`], [`crate::WriteCollectionOptions::log_and_continue`]
            /// to log each failure and keep collecting, or
            /// [`crate::WriteCollectionOptions::with_error_handler`] to make a per-error
            /// continue-or-stop decision.
            #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
            pub fn collect_lines_into_write_mapped<W, B, H>(
                &self,
                write: W,
                mapper: impl Fn(::std::borrow::Cow<'_, str>) -> B + Send + Sync + 'static,
                options: $crate::LineParsingOptions,
                mode: $crate::LineWriteMode,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> $crate::Consumer<W>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                B: AsRef<[u8]> + Send + 'static,
                H: $crate::SinkWriteErrorHandler,
            {
                __consumer_call!(
                    Direct,
                    self,
                    $crate::output_stream::consumer::visitors::write::collect_lines_into_write_mapped,
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
                    $crate::output_stream::consumer::visitors::wait::wait_for_line_bounded(
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
            /// Tries to drive the provided synchronous [`StreamVisitor`](crate::StreamVisitor)
            /// over this stream.
            ///
            /// See [`consume_with`](Self::consume_with) — wait, that's the infallible variant on
            /// the other backends. This method has the same role: it returns a
            /// [`Consumer`](crate::Consumer) that owns the spawned task driving the visitor.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn consume_with<V>(
                &self,
                visitor: V,
            ) -> Result<$crate::Consumer<V::Output>, $crate::StreamConsumerError>
            where
                V: $crate::StreamVisitor,
            {
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::consumer::consume_with,
                    visitor,
                )
            }

            /// Tries to drive the provided asynchronous
            /// [`AsyncStreamVisitor`](crate::AsyncStreamVisitor) over this stream.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn consume_with_async<V>(
                &self,
                visitor: V,
            ) -> Result<$crate::Consumer<V::Output>, $crate::StreamConsumerError>
            where
                V: $crate::AsyncStreamVisitor,
            {
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::consumer::consume_with_async,
                    visitor,
                )
            }

            /// Tries to inspect chunks of output from the stream without storing them.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn inspect_chunks(
                &self,
                f: impl FnMut($crate::Chunk) -> $crate::Next + Send + 'static,
            ) -> Result<$crate::Inspector, $crate::StreamConsumerError> {
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::inspect::inspect_chunks,
                    f,
                )
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
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::inspect::inspect_chunks_async,
                    f,
                )
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
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::inspect::inspect_lines,
                    f,
                    options,
                )
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
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::inspect::inspect_lines_async,
                    f,
                    options,
                )
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
            ) -> Result<$crate::Consumer<S>, $crate::StreamConsumerError> {
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::collect::collect_chunks,
                    into,
                    collect,
                )
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
            ) -> Result<$crate::Consumer<S>, $crate::StreamConsumerError>
            where
                S: $crate::Sink,
                C: $crate::AsyncChunkCollector<S>,
            {
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::collect::collect_chunks_async,
                    into,
                    collect,
                )
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
            ) -> Result<$crate::Consumer<S>, $crate::StreamConsumerError> {
                assert!(
                    options.max_line_length.bytes() > 0,
                    "LineParsingOptions::max_line_length must be greater than zero"
                );
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::collect::collect_lines,
                    into,
                    collect,
                    options,
                )
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
            ) -> Result<$crate::Consumer<S>, $crate::StreamConsumerError>
            where
                S: $crate::Sink,
                C: $crate::AsyncLineCollector<S>,
            {
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::collect::collect_lines_async,
                    into,
                    collect,
                    options,
                )
            }

            /// Tries to collect chunks into a bounded byte vector.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn collect_chunks_into_vec(
                &self,
                options: $crate::RawCollectionOptions,
            ) -> Result<$crate::Consumer<$crate::CollectedBytes>, $crate::StreamConsumerError> {
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::collect::collect_chunks_into_vec,
                    options,
                )
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
            ) -> Result<$crate::Consumer<$crate::CollectedLines>, $crate::StreamConsumerError> {
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::collect::collect_lines_into_vec,
                    parsing_options,
                    collection_options,
                )
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
            ) -> Result<$crate::Consumer<W>, $crate::StreamConsumerError>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                H: $crate::SinkWriteErrorHandler,
            {
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::write::collect_chunks_into_write,
                    write,
                    write_options,
                )
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
            ) -> Result<$crate::Consumer<W>, $crate::StreamConsumerError>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                H: $crate::SinkWriteErrorHandler,
            {
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::write::collect_lines_into_write,
                    write,
                    options,
                    mode,
                    write_options,
                )
            }

            /// Tries to collect chunks into an async writer after mapping them.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn collect_chunks_into_write_mapped<W, B, H>(
                &self,
                write: W,
                mapper: impl Fn($crate::Chunk) -> B + Send + Sync + 'static,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> Result<$crate::Consumer<W>, $crate::StreamConsumerError>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                B: AsRef<[u8]> + Send + 'static,
                H: $crate::SinkWriteErrorHandler,
            {
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::write::collect_chunks_into_write_mapped,
                    write,
                    mapper,
                    write_options,
                )
            }

            /// Tries to collect lines into an async writer after mapping them.
            ///
            /// # Errors
            ///
            /// Returns [`crate::StreamConsumerError`] if the backend rejects the consumer.
            pub fn collect_lines_into_write_mapped<W, B, H>(
                &self,
                write: W,
                mapper: impl Fn(::std::borrow::Cow<'_, str>) -> B + Send + Sync + 'static,
                options: $crate::LineParsingOptions,
                mode: $crate::LineWriteMode,
                write_options: $crate::WriteCollectionOptions<H>,
            ) -> Result<$crate::Consumer<W>, $crate::StreamConsumerError>
            where
                W: $crate::Sink + tokio::io::AsyncWriteExt + Unpin,
                B: AsRef<[u8]> + Send + 'static,
                H: $crate::SinkWriteErrorHandler,
            {
                __consumer_call!(
                    Fallible,
                    self,
                    $crate::output_stream::consumer::visitors::write::collect_lines_into_write_mapped,
                    write,
                    mapper,
                    options,
                    mode,
                    write_options,
                )
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
                    $crate::output_stream::consumer::visitors::wait::wait_for_line_bounded(
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

    /// A custom synchronous visitor that counts chunks and breaks after `limit`.
    struct CountChunks {
        seen: usize,
        limit: usize,
    }

    impl crate::StreamVisitor for CountChunks {
        type Output = usize;

        fn on_chunk(&mut self, _chunk: Chunk) -> Next {
            self.seen += 1;
            if self.seen >= self.limit {
                Next::Break
            } else {
                Next::Continue
            }
        }

        fn into_output(self) -> usize {
            self.seen
        }
    }

    #[tokio::test]
    async fn consume_with_runs_a_custom_sync_visitor_until_break() {
        let (stream, tx) = stream_with_sender();
        let consumer = stream.consume_with(CountChunks { seen: 0, limit: 2 });

        tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(b"first"))))
            .await
            .unwrap();
        tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(b"second"))))
            .await
            .unwrap();
        // Third chunk should never be observed because the visitor breaks at 2.
        tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(b"third"))))
            .await
            .unwrap();
        tx.send(StreamEvent::Eof).await.unwrap();

        let observed = consumer.wait().await.unwrap();
        assert_that!(observed).is_equal_to(2);
    }

    /// A custom async visitor that pushes every chunk to a Vec via .await.
    struct ForwardChunksAsync {
        tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    }

    impl crate::AsyncStreamVisitor for ForwardChunksAsync {
        type Output = ();

        async fn on_chunk(&mut self, chunk: Chunk) -> Next {
            match self.tx.send(chunk.as_ref().to_vec()).await {
                Ok(()) => Next::Continue,
                Err(_) => Next::Break,
            }
        }

        fn into_output(self) {}
    }

    #[tokio::test]
    async fn consume_with_async_runs_a_custom_async_visitor_to_eof() {
        let (stream, tx) = stream_with_sender();
        let (forwarded_tx, mut forwarded_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let consumer = stream.consume_with_async(ForwardChunksAsync { tx: forwarded_tx });

        tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(b"alpha"))))
            .await
            .unwrap();
        tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(b"beta"))))
            .await
            .unwrap();
        tx.send(StreamEvent::Eof).await.unwrap();

        consumer.wait().await.unwrap();

        let mut forwarded = Vec::new();
        while let Some(bytes) = forwarded_rx.recv().await {
            forwarded.push(bytes);
        }
        assert_that!(forwarded).is_equal_to(vec![b"alpha".to_vec(), b"beta".to_vec()]);
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
