//! The single source of truth for the visitor-factory methods exposed by every output-stream
//! backend.
//!
//! Each backend (`broadcast`, `single_subscriber`) invokes
//! [`impl_consumer_factories!`](impl_consumer_factories) inside its own inherent `impl` block,
//! so the methods stay inherent on each concrete type and remain discoverable via IDE
//! autocomplete without any extra trait `use`. Per-backend variation in return types is
//! controlled by a local `FactoryReturn<T>` type alias defined at the top of each backend
//! module:
//!
//! - `broadcast` defines `type FactoryReturn<T> = T;` — factories return their `Consumer<…>`
//!   directly.
//! - `single_subscriber` defines
//!   `type FactoryReturn<T> = Result<T, StreamConsumerError>;` — factories return a `Result`
//!   because the subscription can be rejected if a consumer is already active.
//!
//! Each method body delegates to `self.consume_with(...)` or `self.consume_with_async(...)`,
//! which already produce the matching return type for their backend, so the macro body
//! type-checks identically for both invocations.

/// Emits the visitor-factory methods (`inspect_*`, `collect_*`) inside the surrounding
/// inherent `impl` block.
///
/// The expansion expects the following items to be in scope at the call site:
///
/// - `FactoryReturn<T>` — the per-backend return-type alias (see module docs).
/// - `consume_with(visitor)` and `consume_with_async(visitor)` inherent methods on `Self`,
///   each returning `FactoryReturn<Consumer<…>>`.
///
/// `wait_for_line` is intentionally not emitted by this macro: it does not go through
/// `consume_with`, builds its timeout-bounded future directly, and the two backends differ in
/// how they acquire a subscription. Each backend keeps its own `wait_for_line` inherent
/// method.
///
/// The macro suppresses `clippy::missing_errors_doc` on every emitted method. A shared
/// `# Errors` section is impossible here: only single-subscriber backends actually return a
/// `Result`, while broadcast's `FactoryReturn<T> = T` makes the wrapper vanish and any text
/// about [`StreamConsumerError`](crate::StreamConsumerError) would mislead broadcast readers.
/// The single-subscriber error case is documented once on
/// [`SingleSubscriberOutputStream::consume_with`](crate::SingleSubscriberOutputStream::consume_with),
/// and each variant of [`StreamConsumerError`](crate::StreamConsumerError) carries its own
/// failure-condition docs.
macro_rules! impl_consumer_factories {
    () => {
        /// Inspects chunks of output from the stream without storing them.
        ///
        /// The provided closure is called for each chunk of data. Return [`Next::Continue`] to
        /// keep processing or [`Next::Break`] to stop.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn inspect_chunks(
            &self,
            f: impl FnMut($crate::Chunk) -> $crate::Next + Send + 'static,
        ) -> FactoryReturn<$crate::Consumer<()>> {
            self.consume_with(
                $crate::output_stream::visitors::inspect::InspectChunks::builder()
                    .f(f)
                    .build(),
            )
        }

        /// Inspects chunks of output from the stream without storing them, using an async
        /// closure.
        ///
        /// The provided async closure is called for each chunk of data. Return
        /// [`Next::Continue`] to keep processing or [`Next::Break`] to stop.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn inspect_chunks_async<Fut>(
            &self,
            f: impl FnMut($crate::Chunk) -> Fut + Send + 'static,
        ) -> FactoryReturn<$crate::Consumer<()>>
        where
            Fut: ::std::future::Future<Output = $crate::Next> + Send + 'static,
        {
            self.consume_with_async(
                $crate::output_stream::visitors::inspect::InspectChunksAsync::builder()
                    .f(f)
                    .build(),
            )
        }

        /// Inspects lines of output from the stream without storing them.
        ///
        /// The provided closure is called for each line. Return [`Next::Continue`] to keep
        /// processing or [`Next::Break`] to stop.
        ///
        /// # Panics
        ///
        /// Panics if `options.max_line_length` is zero.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn inspect_lines(
            &self,
            f: impl FnMut(::std::borrow::Cow<'_, str>) -> $crate::Next + Send + 'static,
            options: $crate::LineParsingOptions,
        ) -> FactoryReturn<$crate::Consumer<()>> {
            self.consume_with($crate::output_stream::line::adapter::LineAdapter::new(
                options,
                $crate::output_stream::visitors::inspect::InspectLineSink::new(f),
            ))
        }

        /// Inspects lines of output from the stream without storing them, using an async
        /// closure.
        ///
        /// The provided async closure is called for each line. Return [`Next::Continue`] to
        /// keep processing or [`Next::Break`] to stop.
        ///
        /// # Panics
        ///
        /// Panics if `options.max_line_length` is zero.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn inspect_lines_async<Fut>(
            &self,
            f: impl FnMut(::std::borrow::Cow<'_, str>) -> Fut + Send + 'static,
            options: $crate::LineParsingOptions,
        ) -> FactoryReturn<$crate::Consumer<()>>
        where
            Fut: ::std::future::Future<Output = $crate::Next> + Send + 'static,
        {
            self.consume_with_async(
                $crate::output_stream::line::adapter::LineAdapter::new(
                    options,
                    $crate::output_stream::visitors::inspect::InspectLineSinkAsync::new(f),
                ),
            )
        }

        /// Collects chunks from the stream into a sink.
        ///
        /// The provided closure is called for each chunk, with mutable access to the sink.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn collect_chunks<S: $crate::Sink>(
            &self,
            into: S,
            collect: impl FnMut($crate::Chunk, &mut S) + Send + 'static,
        ) -> FactoryReturn<$crate::Consumer<S>> {
            self.consume_with(
                $crate::output_stream::visitors::collect::CollectChunks::builder()
                    .sink(into)
                    .f(collect)
                    .build(),
            )
        }

        /// Collects chunks from the stream into a sink using an async collector.
        ///
        /// The provided async collector is called for each chunk, with mutable access to the
        /// sink.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn collect_chunks_async<S, C>(
            &self,
            into: S,
            collect: C,
        ) -> FactoryReturn<$crate::Consumer<S>>
        where
            S: $crate::Sink,
            C: $crate::AsyncChunkCollector<S>,
        {
            self.consume_with_async(
                $crate::output_stream::visitors::collect::CollectChunksAsync::builder()
                    .sink(into)
                    .collector(collect)
                    .build(),
            )
        }

        /// Collects lines from the stream into a sink.
        ///
        /// The provided closure is called for each line, with mutable access to the sink.
        /// Return [`Next::Continue`] to keep processing or [`Next::Break`] to stop.
        ///
        /// # Panics
        ///
        /// Panics if `options.max_line_length` is zero.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn collect_lines<S: $crate::Sink>(
            &self,
            into: S,
            collect: impl FnMut(::std::borrow::Cow<'_, str>, &mut S) -> $crate::Next + Send + 'static,
            options: $crate::LineParsingOptions,
        ) -> FactoryReturn<$crate::Consumer<S>> {
            self.consume_with($crate::output_stream::line::adapter::LineAdapter::new(
                options,
                $crate::output_stream::visitors::collect::CollectLineSink::new(
                    into, collect,
                ),
            ))
        }

        /// Collects lines from the stream into a sink using an async collector.
        ///
        /// The provided async collector is called for each line, with mutable access to the
        /// sink. Return [`Next::Continue`] to keep processing or [`Next::Break`] to stop.
        ///
        /// # Panics
        ///
        /// Panics if `options.max_line_length` is zero.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn collect_lines_async<S, C>(
            &self,
            into: S,
            collect: C,
            options: $crate::LineParsingOptions,
        ) -> FactoryReturn<$crate::Consumer<S>>
        where
            S: $crate::Sink,
            C: $crate::AsyncLineCollector<S>,
        {
            self.consume_with_async(
                $crate::output_stream::line::adapter::LineAdapter::new(
                    options,
                    $crate::output_stream::visitors::collect::CollectLineSinkAsync::new(
                        into, collect,
                    ),
                ),
            )
        }

        /// Convenience method to collect chunks into a bounded byte vector.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn collect_chunks_into_vec(
            &self,
            options: $crate::RawCollectionOptions,
        ) -> FactoryReturn<$crate::Consumer<$crate::CollectedBytes>> {
            self.consume_with(
                $crate::output_stream::visitors::collect::CollectChunks::builder()
                    .sink($crate::CollectedBytes::new())
                    .f(move |chunk: $crate::Chunk, sink: &mut $crate::CollectedBytes| {
                        sink.push_chunk(chunk.as_ref(), options);
                    })
                    .build(),
            )
        }

        /// Convenience method to collect lines into a line buffer.
        ///
        /// `parsing_options.max_line_length` must be non-zero unless `collection_options` is
        /// [`LineCollectionOptions::TrustedUnbounded`].
        ///
        /// # Panics
        ///
        /// Panics if `parsing_options.max_line_length` is zero and bounded collection is used.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn collect_lines_into_vec(
            &self,
            parsing_options: $crate::LineParsingOptions,
            collection_options: $crate::LineCollectionOptions,
        ) -> FactoryReturn<$crate::Consumer<$crate::CollectedLines>> {
            self.consume_with($crate::output_stream::line::adapter::LineAdapter::new(
                parsing_options,
                $crate::output_stream::visitors::collect::CollectLineSink::new(
                    $crate::CollectedLines::new(),
                    move |line: ::std::borrow::Cow<'_, str>, sink: &mut $crate::CollectedLines| {
                        sink.push_line(line.into_owned(), collection_options);
                        $crate::Next::Continue
                    },
                ),
            ))
        }

        /// Collects chunks into an async writer.
        ///
        /// Sink write failures are handled according to `write_options`. Use
        /// [`WriteCollectionOptions::fail_fast`] to stop collection and surface the
        /// [`SinkWriteError`] as the inner `Err` of the resulting `Result<W, SinkWriteError>`,
        /// [`WriteCollectionOptions::log_and_continue`] to log each failure and keep
        /// collecting, or [`WriteCollectionOptions::with_error_handler`] to make a per-error
        /// continue-or-stop decision.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn collect_chunks_into_write<W, H>(
            &self,
            write: W,
            write_options: $crate::WriteCollectionOptions<H>,
        ) -> FactoryReturn<$crate::Consumer<::std::result::Result<W, $crate::SinkWriteError>>>
        where
            W: $crate::Sink + ::tokio::io::AsyncWriteExt + Unpin,
            H: $crate::SinkWriteErrorHandler,
        {
            self.consume_with_async(
                $crate::output_stream::visitors::write::WriteChunks::builder()
                    .stream_name($crate::output_stream::OutputStream::name(self))
                    .writer(write)
                    .error_handler(write_options.into_error_handler())
                    .mapper((|chunk: $crate::Chunk| chunk) as fn($crate::Chunk) -> $crate::Chunk)
                    .error(None)
                    .build(),
            )
        }

        /// Collects lines into an async writer.
        ///
        /// Parsed lines no longer include their trailing newline byte, so `mode` controls
        /// whether a `\n` delimiter should be reintroduced for each emitted line.
        ///
        /// Sink write failures are handled according to `write_options` — see
        /// [`collect_chunks_into_write`](Self::collect_chunks_into_write) for the
        /// failure-handling options.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn collect_lines_into_write<W, H>(
            &self,
            write: W,
            options: $crate::LineParsingOptions,
            mode: $crate::LineWriteMode,
            write_options: $crate::WriteCollectionOptions<H>,
        ) -> FactoryReturn<$crate::Consumer<::std::result::Result<W, $crate::SinkWriteError>>>
        where
            W: $crate::Sink + ::tokio::io::AsyncWriteExt + Unpin,
            H: $crate::SinkWriteErrorHandler,
        {
            self.consume_with_async(
                $crate::output_stream::line::adapter::LineAdapter::new(
                    options,
                    $crate::output_stream::visitors::write::WriteLineSink::new(
                        $crate::output_stream::OutputStream::name(self),
                        write,
                        write_options.into_error_handler(),
                        (|line: ::std::borrow::Cow<'_, str>| line.into_owned())
                            as fn(::std::borrow::Cow<'_, str>) -> ::std::string::String,
                        mode,
                    ),
                ),
            )
        }

        /// Collects chunks into an async writer after mapping them with the provided function.
        ///
        /// Sink write failures are handled according to `write_options` — see
        /// [`collect_chunks_into_write`](Self::collect_chunks_into_write) for the
        /// failure-handling options.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn collect_chunks_into_write_mapped<W, B, H>(
            &self,
            write: W,
            mapper: impl Fn($crate::Chunk) -> B + Send + Sync + 'static,
            write_options: $crate::WriteCollectionOptions<H>,
        ) -> FactoryReturn<$crate::Consumer<::std::result::Result<W, $crate::SinkWriteError>>>
        where
            W: $crate::Sink + ::tokio::io::AsyncWriteExt + Unpin,
            B: AsRef<[u8]> + Send + 'static,
            H: $crate::SinkWriteErrorHandler,
        {
            self.consume_with_async(
                $crate::output_stream::visitors::write::WriteChunks::builder()
                    .stream_name($crate::output_stream::OutputStream::name(self))
                    .writer(write)
                    .error_handler(write_options.into_error_handler())
                    .mapper(mapper)
                    .error(None)
                    .build(),
            )
        }

        /// Collects lines into an async writer after mapping them with the provided function.
        ///
        /// `mode` applies after `mapper`: choose [`LineWriteMode::AsIs`] when the mapped
        /// output already contains delimiters, or [`LineWriteMode::AppendLf`] to append `\n`
        /// after each mapped line.
        ///
        /// Sink write failures are handled according to `write_options` — see
        /// [`collect_chunks_into_write`](Self::collect_chunks_into_write) for the
        /// failure-handling options.
        #[allow(clippy::missing_errors_doc)]
        #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
        pub fn collect_lines_into_write_mapped<W, B, H>(
            &self,
            write: W,
            mapper: impl Fn(::std::borrow::Cow<'_, str>) -> B + Send + Sync + 'static,
            options: $crate::LineParsingOptions,
            mode: $crate::LineWriteMode,
            write_options: $crate::WriteCollectionOptions<H>,
        ) -> FactoryReturn<$crate::Consumer<::std::result::Result<W, $crate::SinkWriteError>>>
        where
            W: $crate::Sink + ::tokio::io::AsyncWriteExt + Unpin,
            B: AsRef<[u8]> + Send + 'static,
            H: $crate::SinkWriteErrorHandler,
        {
            self.consume_with_async(
                $crate::output_stream::line::adapter::LineAdapter::new(
                    options,
                    $crate::output_stream::visitors::write::WriteLineSink::new(
                        $crate::output_stream::OutputStream::name(self),
                        write,
                        write_options.into_error_handler(),
                        mapper,
                        mode,
                    ),
                ),
            )
        }
    };
}

pub(crate) use impl_consumer_factories;
