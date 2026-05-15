//! Platform-independent long-running program that correctly handles graceful termination signals.

#[tokio::main(flavor = "current_thread")]
async fn main() {
    wait_for_graceful_signal().await;
    std::process::exit(1);
}

#[cfg(windows)]
async fn wait_for_graceful_signal() {
    let mut ctrl_break = tokio::signal::windows::ctrl_break().expect("install CTRL_BREAK handler");
    ctrl_break.recv().await;
}

#[cfg(unix)]
async fn wait_for_graceful_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => {},
        _ = sigint.recv() => {},
    }
}
