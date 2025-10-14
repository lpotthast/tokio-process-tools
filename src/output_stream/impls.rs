macro_rules! impl_inspect_chunks {
    ($receiver:expr, $f:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Inspector {
            task: Some(tokio::spawn(async move {
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        Some(chunk) => {
                            if let Next::Break = $f(chunk) {
                                break 'outer;
                            }
                        }
                        None => {
                            // EOF reached.
                            break 'outer;
                        }
                    }
                });
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_inspect_chunks;

macro_rules! impl_inspect_lines {
    ($receiver:expr, $f:ident, $options:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Inspector {
            task: Some(tokio::spawn(async move {
                let mut line_buffer = bytes::BytesMut::new();
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        Some(chunk) => {
                            let lr = LineReader::new(chunk.as_ref(), &mut line_buffer, $options);
                            for line in lr {
                                // TODO: Pass Cow instead of String.
                                let line = String::from_utf8_lossy(&line).to_string();
                                let next = $f(line);
                                if next == Next::Break {
                                    break 'outer;
                                }
                            }
                        }
                        None => {
                            if !line_buffer.is_empty() {
                                // TODO: Pass Cow instead of String.
                                $f(String::from_utf8_lossy(&line_buffer).to_string());
                            }
                            break 'outer;
                        }
                    }
                });
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_inspect_lines;

macro_rules! impl_inspect_lines_async {
    ($receiver:expr, $f:ident, $options:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Inspector {
            task: Some(tokio::spawn(async move {
                let mut line_buffer = bytes::BytesMut::new();
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        Some(chunk) => {
                            let lr = LineReader::new(chunk.as_ref(), &mut line_buffer, $options);
                            for line in lr {
                                // TODO: Pass Cow instead of String.
                                let line = String::from_utf8_lossy(&line).to_string();
                                match $f(line).await {
                                    Next::Continue => {}
                                    Next::Break => break 'outer,
                                }
                            }
                        }
                        None => {
                            if !line_buffer.is_empty() {
                                // TODO: Pass Cow instead of String.
                                $f(String::from_utf8_lossy(&line_buffer).to_string());
                            }
                            break 'outer;
                        }
                    }
                });
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_inspect_lines_async;

macro_rules! impl_collect_chunks {
    ($receiver:expr, $collect:ident, $sink:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Collector {
            task: Some(tokio::spawn(async move {
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        Some(chunk) => {
                            let mut write_guard = $sink.write().await;
                            $collect(chunk, &mut (*write_guard));
                        }
                        None => {
                            // EOF reached.
                            break 'outer;
                        }
                    }
                });
                Arc::try_unwrap($sink).expect("single owner").into_inner()
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_collect_chunks;

macro_rules! impl_collect_lines {
    ($receiver:expr, $collect:ident, $options:ident, $sink:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Collector {
            task: Some(tokio::spawn(async move {
                let mut line_buffer = bytes::BytesMut::new();
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        Some(chunk) => {
                            let mut write_guard = $sink.write().await;
                            let sink = &mut *write_guard;
                            let lr = LineReader::new(chunk.as_ref(), &mut line_buffer, $options);
                            for line in lr {
                                // TODO: Pass Cow instead of String.
                                let line = String::from_utf8_lossy(&line).to_string();
                                match $collect(line, sink) {
                                    Next::Continue => {}
                                    Next::Break => break 'outer,
                                }
                            }
                        }
                        None => {
                            // EOF reached.
                            break 'outer;
                        }
                    }
                });
                Arc::try_unwrap($sink).expect("single owner").into_inner()
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_collect_lines;

macro_rules! impl_collect_chunks_async {
    ($receiver:expr, $collect:ident, $sink:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Collector {
            task: Some(tokio::spawn(async move {
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        Some(chunk) => {
                            let result = {
                                let mut write_guard = $sink.write().await;
                                let fut = $collect(chunk, &mut *write_guard);
                                fut.await
                            };
                            match result {
                                Next::Continue => {},
                                Next::Break => break 'outer,
                            }
                        }
                        None => {
                            // EOF reached.
                            break 'outer;
                        }
                    }
                });
                Arc::try_unwrap($sink).expect("single owner").into_inner()
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_collect_chunks_async;

macro_rules! impl_collect_lines_async {
    ($receiver:expr, $collect:ident, $options:ident, $sink:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Collector {
            task: Some(tokio::spawn(async move {
                let mut line_buffer = bytes::BytesMut::new();
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        Some(chunk) => {
                            let mut write_guard = $sink.write().await;
                            let sink = &mut *write_guard;
                            let lr = LineReader::new(chunk.as_ref(), &mut line_buffer, $options);
                            for line in lr {
                                // TODO: Pass Cow instead of String.
                                let line = String::from_utf8_lossy(&line).to_string();
                                match $collect(line, sink).await {
                                    Next::Continue => {},
                                    Next::Break => {
                                        break 'outer
                                    },
                                }
                            }
                        }
                        None => {
                            // EOF reached.
                            if !line_buffer.is_empty() {
                                let mut write_guard = $sink.write().await;
                                let sink = &mut *write_guard;
                                match $collect(String::from_utf8_lossy(&line_buffer).to_string(), sink).await {
                                    Next::Continue | Next::Break => {
                                        /* irrelevant, we always break on EOF */
                                    }
                                }
                            }
                            break 'outer;
                        }
                    }
                });
                Arc::try_unwrap($sink).expect("single owner").into_inner()
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_collect_lines_async;
