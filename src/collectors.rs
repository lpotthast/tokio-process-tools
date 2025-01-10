use crate::collector::AsyncCollectFn;
use crate::Sink;
use tokio::io::AsyncWriteExt;

pub fn to_writer_with_newline<S: Sink + AsyncWriteExt + Unpin>(
) -> impl Fn(String, &mut S) -> AsyncCollectFn<'_> {
    |line, sink| {
        Box::pin(async move {
            sink.write_all(line.as_bytes()).await?;
            sink.write_all("\n".as_bytes()).await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::collectors::to_writer_with_newline;
    use crate::ProcessHandle;
    use std::process::Stdio;

    #[tokio::test]
    async fn write_with_predefined_collector() {
        let child = tokio::process::Command::new("ls")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let mut handle = ProcessHandle::new_from_child_with_piped_io("ls", child);

        let file = tokio::fs::File::create(
            std::env::temp_dir()
                .join("tokio_process_tools_test_write_with_predefined_collector.txt"),
        )
        .await
        .unwrap();

        let collector = handle
            .stdout()
            .collect_async(file, to_writer_with_newline());

        let _exit_status = handle.wait().await.unwrap();
        let _file = collector.abort().await.unwrap();
    }
}
