use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use tempdir::TempDir;
use tokio::io::Lines;
use tokio::process::{ChildStderr, ChildStdout};

use tokio::sync::oneshot::Sender;
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::{
    io::BufReader,
    process::{Child, Command},
};
use tracing::{debug, instrument};

use crate::errors::{ProcessCapture, TmpPostgrustError, TmpPostgrustResult};
use crate::search::find_postgresql_command;

/// Limit the total processes that can be running at any one time.
pub(crate) static MAX_CONCURRENT_PROCESSES: Semaphore = Semaphore::const_new(8);

#[instrument(skip(command, fail))]
async fn exec_process(
    command: &mut Command,
    fail: impl FnOnce(ProcessCapture) -> TmpPostgrustError,
) -> TmpPostgrustResult<()> {
    debug!("running command: {:?}", command);

    let output = command
        .output()
        .await
        .map_err(|err| TmpPostgrustError::ExecSubprocessFailed {
            source: err,
            command: format!("{:?}", command),
        })?;

    if output.status.success() {
        for line in String::from_utf8(output.stdout).unwrap().lines() {
            debug!("{}", line);
        }
        Ok(())
    } else {
        Err(fail(ProcessCapture {
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }))
    }
}

#[instrument]
pub(crate) fn start_postgres_subprocess(
    data_directory: &'_ Path,
    port: u32,
) -> TmpPostgrustResult<Child> {
    let postgres_path =
        find_postgresql_command("bin", "postgres").expect("failed to find postgres");

    Command::new(postgres_path)
        .env("PGDATA", data_directory.to_str().unwrap())
        .arg("-p")
        .arg(port.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(TmpPostgrustError::SpawnSubprocessFailed)
}

#[instrument]
pub(crate) async fn exec_init_db(data_directory: &'_ Path) -> TmpPostgrustResult<()> {
    let initdb_path = find_postgresql_command("bin", "initdb").expect("failed to find initdb");

    debug!("Initializing database in: {:?}", data_directory);
    exec_process(
        &mut Command::new(initdb_path)
            .env("PGDATA", data_directory.to_str().unwrap())
            .arg("--username=postgres"),
        TmpPostgrustError::InitDBFailed,
    )
    .await
}

#[instrument]
pub(crate) async fn exec_copy_dir(src_dir: &'_ Path, dst_dir: &'_ Path) -> TmpPostgrustResult<()> {
    for read_dir in src_dir
        .read_dir()
        .map_err(TmpPostgrustError::CopyCachedInitDBFailedFileNotFound)?
    {
        let mut cmd = Command::new("cp");
        #[cfg(target_os = "macos")]
        cmd.arg("-R")
            .arg("-c")
            .arg(
                read_dir
                    .map_err(TmpPostgrustError::CopyCachedInitDBFailedFileNotFound)?
                    .path(),
            )
            .arg(dst_dir);
        #[cfg(not(target_os = "macos"))]
        cmd.arg("-R")
            .arg("--reflink=auto")
            .arg(
                read_dir
                    .map_err(TmpPostgrustError::CopyCachedInitDBFailedFileNotFound)?
                    .path(),
            )
            .arg(dst_dir);
        exec_process(&mut cmd, TmpPostgrustError::CopyCachedInitDBFailed).await?;
    }
    Ok(())
}

#[instrument]
pub(crate) async fn exec_create_db(
    socket: &'_ Path,
    port: u32,
    owner: &'_ str,
    dbname: &'_ str,
) -> TmpPostgrustResult<()> {
    exec_process(
        &mut Command::new("createdb")
            .arg("-h")
            .arg(socket)
            .arg("-p")
            .arg(port.to_string())
            .arg("-U")
            .arg("postgres")
            .arg("-O")
            .arg(owner)
            .arg("--echo")
            .arg(dbname),
        TmpPostgrustError::CreateDBFailed,
    )
    .await
}

#[instrument]
pub(crate) async fn exec_create_user(
    socket: &'_ Path,
    port: u32,
    username: &'_ str,
) -> TmpPostgrustResult<()> {
    exec_process(
        &mut Command::new("createuser")
            .arg("-h")
            .arg(socket)
            .arg("-p")
            .arg(port.to_string())
            .arg("-U")
            .arg("postgres")
            .arg("--superuser")
            .arg("--echo")
            .arg(username),
        TmpPostgrustError::CreateDBFailed,
    )
    .await
}

/// ProcessGuard represents a postgresql process that is running in the background.
/// once the guard is dropped the process will be killed.
pub struct ProcessGuard {
    /// Allows users to read stdout by line for debugging.
    pub stdout_reader: Option<Lines<BufReader<ChildStdout>>>,
    /// Allows users to read stderr by line for debugging.
    pub stderr_reader: Option<Lines<BufReader<ChildStderr>>>,
    /// Connection string for connecting to the temporary postgresql instance.
    pub connection_string: String,

    // Signal that the postgres process should be killed.
    pub(crate) send_done: Option<Sender<()>>,
    // Prevent the data directory from being dropped while
    // the process is running.
    pub(crate) _data_directory: TempDir,
    // Prevent socket directory from being dropped while
    // the process is running.
    pub(crate) _socket_dir: Arc<TempDir>,
    // Limit the total concurrent processes.
    pub(crate) _process_permit: SemaphorePermit<'static>,
}

/// Signal that the process needs to end.
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(sender) = self.send_done.take() {
            sender
                .send(())
                .expect("failed to signal postgresql process should be killed.");
        }
    }
}
