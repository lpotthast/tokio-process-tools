//! Platform-independent long-running program that correctly handles graceful termination signals.

#[tokio::main(flavor = "current_thread")]
async fn main() {
    wait_for_graceful_signal().await;

    if let Some(marker) = std::env::var_os("TOKIO_PROCESS_TOOLS_GRACEFUL_MARKER") {
        std::fs::write(marker, "graceful").expect("write graceful-shutdown marker");
    }

    std::process::exit(1);
}

#[cfg(windows)]
async fn wait_for_graceful_signal() {
    let mut ctrl_break = tokio::signal::windows::ctrl_break().expect("install CTRL_BREAK handler");
    announce_readiness();
    ctrl_break.recv().await;
}

#[cfg(unix)]
async fn wait_for_graceful_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    announce_readiness();
    tokio::select! {
        _ = sigterm.recv() => {},
        _ = sigint.recv() => {},
    }
}

/// Printed once the graceful-signal handlers are installed, so tests can wait for this line
/// before delivering a signal instead of racing the handler installation.
fn announce_readiness() {
    println!("ready");
}
