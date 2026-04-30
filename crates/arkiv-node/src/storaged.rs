//! Supervision for an optional arkiv-storaged child process.

use std::{future::Future, path::PathBuf, process::Stdio};

use eyre::{Result, WrapErr, eyre};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::{Child, Command},
    task::JoinHandle,
};

/// Running arkiv-storaged subprocess.
pub struct ArkivStoragedProcess {
    child: Child,
    pid: Option<u32>,
    stdout_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
}

impl ArkivStoragedProcess {
    /// Start and supervise arkiv-storaged.
    pub fn start(path: PathBuf, args: Option<String>) -> Result<Self> {
        let args = split_args(args.as_deref());

        tracing::info!(
            path = %path.display(),
            args = ?args,
            "starting arkiv-storaged subprocess",
        );

        let mut command = Command::new(&path);
        command
            .args(&args)
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
            args = ?args,
            pid = ?pid,
            "arkiv-storaged subprocess started",
        );

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_task = stdout.map(|stdout| pipe_output("ARKIV-STORAGED-STDOUT", stdout, false));
        let stderr_task = stderr.map(|stderr| pipe_output("ARKIV-STORAGED-STDERR", stderr, true));

        Ok(Self {
            child,
            pid,
            stdout_task,
            stderr_task,
        })
    }

    /// Confirm the subprocess is still running.
    pub fn ensure_running(&mut self) -> Result<()> {
        match self
            .child
            .try_wait()
            .wrap_err("failed to poll arkiv-storaged subprocess")?
        {
            Some(status) => storaged_exit_error(self.pid, status),
            None => Ok(()),
        }
    }

    /// Stop the subprocess.
    pub async fn shutdown(mut self) -> Result<()> {
        tracing::info!(
            pid = ?self.pid,
            "stopping arkiv-storaged subprocess because arkiv-node is exiting",
        );
        self.shutdown_child().await?;
        self.wait_for_output().await;
        Ok(())
    }

    /// Wait for arkiv-node to exit while treating any arkiv-storaged exit as fatal.
    pub async fn run_until_node_exit<F>(mut self, node_exit: F) -> Result<()>
    where
        F: Future<Output = Result<()>>,
    {
        tokio::pin!(node_exit);

        let result = tokio::select! {
            node_result = &mut node_exit => {
                tracing::info!(
                    pid = ?self.pid,
                    "stopping arkiv-storaged subprocess because arkiv-node is exiting",
                );
                self.shutdown_child().await?;
                node_result
            }
            status = self.child.wait() => {
                let status = status.wrap_err("failed to wait for arkiv-storaged subprocess")?;
                storaged_exit_error(self.pid, status)
            }
        };

        self.wait_for_output().await;
        result
    }

    async fn shutdown_child(&mut self) -> Result<()> {
        if let Some(status) = self
            .child
            .try_wait()
            .wrap_err("failed to poll arkiv-storaged subprocess")?
        {
            tracing::info!(
                pid = ?self.pid,
                status = %status,
                "arkiv-storaged subprocess already stopped",
            );
            return Ok(());
        }

        tracing::info!(pid = ?self.pid, "terminating arkiv-storaged subprocess");
        self.child
            .start_kill()
            .wrap_err("failed to kill arkiv-storaged subprocess")?;
        let status = self
            .child
            .wait()
            .await
            .wrap_err("failed to wait for killed arkiv-storaged subprocess")?;
        tracing::info!(
            pid = ?self.pid,
            status = %status,
            "arkiv-storaged subprocess stopped",
        );
        Ok(())
    }

    async fn wait_for_output(&mut self) {
        if let Some(stdout_task) = self.stdout_task.take()
            && let Err(err) = stdout_task.await
        {
            tracing::warn!(%err, "arkiv-storaged stdout task failed");
        }
        if let Some(stderr_task) = self.stderr_task.take()
            && let Err(err) = stderr_task.await
        {
            tracing::warn!(%err, "arkiv-storaged stderr task failed");
        }
    }
}

fn split_args(args: Option<&str>) -> Vec<String> {
    args.unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn storaged_exit_error(pid: Option<u32>, status: std::process::ExitStatus) -> Result<()> {
    if let Some(code) = status.code() {
        tracing::error!(
            pid = ?pid,
            code,
            "arkiv-storaged subprocess exited unexpectedly",
        );
        Err(eyre!(
            "arkiv-storaged subprocess exited unexpectedly with status code {code}",
        ))
    } else {
        tracing::error!(
            pid = ?pid,
            "arkiv-storaged subprocess terminated unexpectedly",
        );
        Err(eyre!("arkiv-storaged subprocess terminated unexpectedly"))
    }
}

fn pipe_output<R>(prefix: &'static str, pipe: R, stderr: bool) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) if stderr => tracing::warn!("{prefix} {line}"),
                Ok(Some(line)) => tracing::info!("{prefix} {line}"),
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(%err, prefix, "failed to read arkiv-storaged output");
                    break;
                }
            }
        }
    })
}
