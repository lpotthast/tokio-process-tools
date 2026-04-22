use super::{ProcessHandle, Stdin};
use crate::output_stream::OutputStream;
use std::mem::ManuallyDrop;
use tokio::process::Child;

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Consumes this handle to provide the wrapped `tokio::process::Child` instance, stdin handle,
    /// and stdout/stderr output streams.
    ///
    /// The returned [`Child`] no longer owns its `stdin` field because this crate separates piped
    /// stdin into [`Stdin`]. Keep the returned [`Stdin`] alive to keep the child's stdin pipe open.
    /// Dropping [`Stdin::Open`] closes the pipe, so the child may observe EOF and exit or otherwise
    /// change behavior.
    pub fn into_inner(mut self) -> (Child, Stdin, Stdout, Stderr) {
        self.must_not_be_terminated();
        let mut this = ManuallyDrop::new(self);

        // SAFETY: `this` is wrapped in `ManuallyDrop`, so moving out fields with `ptr::read` will
        // not cause the original `ProcessHandle` destructor to run. We explicitly drop the fields
        // not returned from this function exactly once before returning the owned parts.
        unsafe {
            let child = std::ptr::read(&raw const this.child);
            let stdin = std::ptr::read(&raw const this.std_in);
            let stdout = std::ptr::read(&raw const this.std_out_stream);
            let stderr = std::ptr::read(&raw const this.std_err_stream);

            std::ptr::drop_in_place(&raw mut this.name);
            std::ptr::drop_in_place(&raw mut this.panic_on_drop);

            (child, stdin, stdout, stderr)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        CollectionOverflowBehavior, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE,
        LineCollectionOptions, LineParsingOptions, NumBytesExt,
    };
    use assertr::prelude::*;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    fn line_collection_options() -> LineCollectionOptions {
        LineCollectionOptions::builder()
            .max_bytes(1.megabytes())
            .max_lines(1024)
            .overflow_behavior(CollectionOverflowBehavior::default())
            .build()
    }

    #[tokio::test]
    async fn test_into_inner_returns_stdin_without_closing_pipe() {
        let cmd = tokio::process::Command::new("cat");
        let process = crate::Process::new(cmd)
            .name("cat")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        let (mut child, mut stdin, stdout, _stderr) = process.into_inner();
        assert_that!(child.stdin.is_none()).is_true();

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_that!(child.try_wait().unwrap().is_none()).is_true();

        let collector =
            stdout.collect_lines_into_vec(LineParsingOptions::default(), line_collection_options());

        let Some(stdin_handle) = stdin.as_mut() else {
            assert_that!(stdin.is_open()).fail("stdin should be returned open");
            return;
        };
        stdin_handle
            .write_all(b"stdin stayed open\n")
            .await
            .unwrap();
        stdin_handle.flush().await.unwrap();

        stdin.close();

        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_that!(status.success()).is_true();

        let collected = collector.wait().await.unwrap();
        assert_that!(collected.lines().len()).is_equal_to(1);
        assert_that!(collected[0].as_str()).is_equal_to("stdin stayed open");
    }

    #[tokio::test]
    async fn test_into_inner_defuses_panic_guard() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");

        let process = crate::Process::new(cmd)
            .name("sleep")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        let (mut child, _stdin, _stdout, _stderr) = process.into_inner();
        child.kill().await.unwrap();
        let _status = child.wait().await.unwrap();
    }

    #[tokio::test]
    async fn test_into_inner_with_owned_name_drops_owned_string() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");

        let process = crate::Process::new(cmd)
            .with_name(format!("sleeper-{}", 7))
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        let (mut child, _stdin, _stdout, _stderr) = process.into_inner();
        child.kill().await.unwrap();
        let _status = child.wait().await.unwrap();
    }
}
