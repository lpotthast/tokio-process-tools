#![allow(missing_docs)]

mod common;

use assertr::prelude::*;
use common::*;
use std::time::Duration;
use unwrap_infallible::UnwrapInfallible;

#[tokio::test(flavor = "multi_thread")]
async fn kill_future_can_be_spawned_on_tokio_task() {
    let mut process = spawn_long_running_process();

    let result = tokio::spawn(async move { process.kill().await })
        .await
        .unwrap();

    assert_that!(result).is_ok();
}

#[tokio::test]
async fn kill_disarms_cleanup_and_panic_guards() {
    use tokio_process_tools::{DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, Process};

    let mut process = Process::new(long_running_command(Duration::from_secs(5)))
        .name("long-running")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .lossy_without_backpressure()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

    assert_that!(process.stdin().is_open()).is_true();
    assert_that!(process.is_drop_armed()).is_true();

    process.kill().await.unwrap();

    assert_that!(process.stdin().is_open()).is_false();
    assert_that!(process.is_drop_disarmed()).is_true();

    drop(process);
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_propagates_to_grandchildren() {
    // Spawn a parent that detaches a long-running grandchild, prints its PID, and waits. After
    // killing the parent, the grandchild must also be gone: on Windows via Job-Object-targeted
    // termination, on Unix via process-group SIGKILL. A PID-only kill would reap only the
    // parent and leave the grandchild ticking.
    use std::sync::Mutex;
    use std::time::Instant;
    use tokio::process::Command;
    use tokio::sync::oneshot;
    use tokio_process_tools::{
        Consumable, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, LineParsingOptions, Next,
        NumBytesExt, ParseLines, Process,
    };

    #[cfg(windows)]
    let cmd = {
        let mut cmd = Command::new("powershell.exe");
        cmd.arg("-NoProfile").arg("-Command").arg(
            "$p = Start-Process -FilePath 'timeout.exe' -ArgumentList '/T','30','/NOBREAK' \
             -PassThru -WindowStyle Hidden; \
             Write-Host $p.Id; \
             Start-Sleep -Seconds 30",
        );
        cmd
    };
    #[cfg(unix)]
    let cmd = {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & echo $!; wait");
        cmd
    };

    let mut process = Process::new(cmd)
        .name("group-kill-test")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .reliable_with_backpressure()
                .replay_last_bytes(64.bytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

    let (tx, rx) = oneshot::channel::<u32>();
    let tx = Mutex::new(Some(tx));
    let _consumer = process
        .stdout()
        .consume(ParseLines::inspect(
            LineParsingOptions::default(),
            move |line| {
                if let Ok(pid) = line.trim().parse::<u32>()
                    && let Some(tx) = tx.lock().unwrap().take()
                {
                    let _ = tx.send(pid);
                }
                Next::Continue
            },
        ))
        .unwrap_infallible();

    let grandchild_pid = tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("grandchild should print its PID")
        .expect("oneshot should resolve once the line is observed");

    process.kill().await.unwrap();

    // After parent kill, poll the platform-specific liveness probe until the grandchild is gone.
    // On Windows the job-object-targeted termination signals the process almost immediately. On
    // Unix the grandchild becomes a zombie until init reaps it; the signal-probe (`kill(_, 0)`)
    // returns ESRCH once init has cleaned up.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if grandchild_is_gone(grandchild_pid) {
            return;
        }
        assert_that!(Instant::now() <= deadline)
            .with_detail_message(format!(
                "grandchild PID {grandchild_pid} still alive 10 s after kill"
            ))
            .is_true();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(windows)]
fn grandchild_is_gone(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    // SAFETY: The PID was just observed via stdout. A null handle means the PID is gone (or has
    // been recycled), which we treat as "exited".
    let handle = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return true;
    }
    // SAFETY: `handle` is a valid process handle returned by `OpenProcess`; `WaitForSingleObject`
    // with timeout 0 only polls; `handle` is closed before this function returns.
    let wait = unsafe { WaitForSingleObject(handle, 0) };
    unsafe {
        CloseHandle(handle);
    }
    wait == WAIT_OBJECT_0
}

#[cfg(unix)]
fn grandchild_is_gone(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid.cast_signed()), None).is_err()
}
