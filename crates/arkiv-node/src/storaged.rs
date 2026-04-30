use eyre::{Result, WrapErr, bail};
use std::{
    path::PathBuf,
    process::{ExitStatus, Stdio},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    sync::oneshot,
    task::JoinHandle,
};

pub struct StoragedProcess {
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<Result<()>>>,
}

impl StoragedProcess {
    pub fn start(path: PathBuf, args: String) -> Result<Self> {
        let argv = split_args(&args);
        tracing::info!(
            path = %path.display(),
            args = ?argv,
            "Arkiv: starting arkiv-storaged subprocess"
        );

        let mut command = Command::new(&path);
        command
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .wrap_err_with(|| format!("failed to start arkiv-storaged at {}", path.display()))?;

        let pid = child.id();
        tracing::info!(
            path = %path.display(),
            args = ?argv,
            pid = ?pid,
            "Arkiv: arkiv-storaged subprocess started"
        );

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_task = stdout.map(|stream| spawn_output_logger(stream, Stream::Stdout));
        let stderr_task = stderr.map(|stream| spawn_output_logger(stream, Stream::Stderr));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let join = tokio::spawn(async move {
            let outcome = tokio::select! {
                status = child.wait() => {
                    StoragedOutcome::Unexpected(status.wrap_err("failed waiting for arkiv-storaged")?)
                }
                _ = shutdown_rx => {
                    tracing::info!(pid = ?pid, "Arkiv: stopping arkiv-storaged subprocess");
                    if let Err(err) = child.start_kill() {
                        tracing::warn!(pid = ?pid, %err, "Arkiv: failed to kill arkiv-storaged subprocess");
                    }
                    StoragedOutcome::Shutdown(
                        child
                            .wait()
                            .await
                            .wrap_err("failed waiting for arkiv-storaged after shutdown")?,
                    )
                }
            };

            join_output_logger(stdout_task, Stream::Stdout).await;
            join_output_logger(stderr_task, Stream::Stderr).await;

            match outcome {
                StoragedOutcome::Unexpected(status) => {
                    let status = describe_status(status);
                    tracing::error!(pid = ?pid, %status, "Arkiv: arkiv-storaged subprocess exited");
                    bail!("arkiv-storaged subprocess exited unexpectedly with {status}")
                }
                StoragedOutcome::Shutdown(status) => {
                    let status = describe_status(status);
                    tracing::info!(pid = ?pid, %status, "Arkiv: arkiv-storaged subprocess stopped");
                    Ok(())
                }
            }
        });

        Ok(Self {
            shutdown: Some(shutdown_tx),
            join: Some(join),
        })
    }

    pub fn into_parts(mut self) -> (StoragedShutdown, JoinHandle<Result<()>>) {
        let shutdown = StoragedShutdown {
            shutdown: self.shutdown.take(),
        };
        let join = self
            .join
            .take()
            .expect("arkiv-storaged supervisor task must exist");
        (shutdown, join)
    }
}

impl Drop for StoragedProcess {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

pub struct StoragedShutdown {
    shutdown: Option<oneshot::Sender<()>>,
}

impl StoragedShutdown {
    pub fn request(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

impl Drop for StoragedShutdown {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

enum StoragedOutcome {
    Unexpected(ExitStatus),
    Shutdown(ExitStatus),
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

fn split_args(args: &str) -> Vec<String> {
    args.split_whitespace().map(ToOwned::to_owned).collect()
}

fn spawn_output_logger<R>(stream: R, stream_kind: Stream) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match stream_kind {
                    Stream::Stdout => tracing::info!("ARKIV-STORAGED-STDOUT {line}"),
                    Stream::Stderr => tracing::warn!("ARKIV-STORAGED-STDERR {line}"),
                },
                Ok(None) => break,
                Err(err) => {
                    match stream_kind {
                        Stream::Stdout => {
                            tracing::warn!(%err, "Arkiv: failed reading arkiv-storaged stdout")
                        }
                        Stream::Stderr => {
                            tracing::warn!(%err, "Arkiv: failed reading arkiv-storaged stderr")
                        }
                    }
                    break;
                }
            }
        }
    })
}

async fn join_output_logger(task: Option<JoinHandle<()>>, stream_kind: Stream) {
    if let Some(task) = task {
        if let Err(err) = task.await {
            match stream_kind {
                Stream::Stdout => {
                    tracing::warn!(%err, "Arkiv: arkiv-storaged stdout logger task failed");
                }
                Stream::Stderr => {
                    tracing::warn!(%err, "Arkiv: arkiv-storaged stderr logger task failed");
                }
            }
        }
    }
}

fn describe_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => status.to_string(),
    }
}
