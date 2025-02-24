/*!
`tmp-postgrust` provides temporary postgresql processes that are cleaned up
after being dropped.


# Inspiration / Similar Projects
- [tmp-postgres](https://github.com/jfischoff/tmp-postgres)
- [testing.postgresql](https://github.com/tk0miya/testing.postgresql)
*/
#![deny(missing_docs)]
#![warn(clippy::all, clippy::pedantic)]

/// Methods for Asynchronous API
#[cfg(feature = "tokio-process")]
pub mod asynchronous;
/// Common Errors
pub mod errors;
mod search;
/// Methods for Synchronous API
pub mod synchronous;

use std::fs::{metadata, set_permissions};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::{fs::File, io::Write};

use lazy_static::lazy_static;
use tempdir::TempDir;
use tracing::{debug, info, instrument};

use crate::errors::{TmpPostgrustError, TmpPostgrustResult};

/// Create a new default instance, initializing the `DEFAULT_POSTGRES_FACTORY` if it
/// does not already exist.
pub fn new_default_process() -> TmpPostgrustResult<synchronous::ProcessGuard> {
    lazy_static! {
        static ref DEFAULT_POSTGRES_FACTORY: TmpPostgrustFactory =
            TmpPostgrustFactory::try_new().unwrap();
    }
    DEFAULT_POSTGRES_FACTORY.new_instance()
}

/// Static factory that can be re-used between tests.
#[cfg(feature = "tokio-process")]
static TOKIO_POSTGRES_FACTORY: tokio::sync::OnceCell<TmpPostgrustFactory> =
    tokio::sync::OnceCell::const_new();

/// Create a new default instance, initializing the `TOKIO_POSTGRES_FACTORY` if it
/// does not already exist.
#[cfg(feature = "tokio-process")]
pub async fn new_default_process_async() -> TmpPostgrustResult<asynchronous::ProcessGuard> {
    let factory = TOKIO_POSTGRES_FACTORY
        .get_or_try_init(TmpPostgrustFactory::try_new_async)
        .await?;
    factory.new_instance_async().await
}

/// Factory for creating new temporary postgresql processes.
#[derive(Debug)]
pub struct TmpPostgrustFactory {
    socket_dir: Arc<TempDir>,
    cache_dir: TempDir,
    config: String,
    next_port: AtomicU32,
}

impl TmpPostgrustFactory {
    /// Build a Postgresql configuration for temporary databases as a String.
    fn build_config(socket_dir: &Path) -> String {
        let mut config = String::new();
        // Minimize chance of running out of shared memory
        config.push_str("shared_buffers = '12MB'\n");
        // Disable TCP connections.
        config.push_str("listen_addresses = ''\n");
        // Listen on UNIX socket.
        config.push_str(&format!(
            "unix_socket_directories = \'{}\'\n",
            socket_dir.to_str().unwrap()
        ));

        config
    }

    /// Try to create a new factory by creating temporary directories and the necessary config.
    #[instrument]
    pub fn try_new() -> TmpPostgrustResult<TmpPostgrustFactory> {
        let socket_dir = TempDir::new("tmp-postgrust-socket")
            .map_err(TmpPostgrustError::CreateSocketDirFailed)?;
        let cache_dir =
            TempDir::new("tmp-postgrust-cache").map_err(TmpPostgrustError::CreateCacheDirFailed)?;

        crate::synchronous::exec_init_db(cache_dir.path())?;

        let config = TmpPostgrustFactory::build_config(socket_dir.path());

        Ok(TmpPostgrustFactory {
            socket_dir: Arc::new(socket_dir),
            cache_dir,
            config,
            next_port: AtomicU32::new(5432),
        })
    }

    /// Try to create a new factory by creating temporary directories and the necessary config.
    #[cfg(feature = "tokio-process")]
    #[instrument]
    pub async fn try_new_async() -> TmpPostgrustResult<TmpPostgrustFactory> {
        let socket_dir = TempDir::new("tmp-postgrust-socket")
            .map_err(TmpPostgrustError::CreateSocketDirFailed)?;
        let cache_dir =
            TempDir::new("tmp-postgrust-cache").map_err(TmpPostgrustError::CreateCacheDirFailed)?;

        crate::asynchronous::exec_init_db(cache_dir.path()).await?;

        let config = TmpPostgrustFactory::build_config(socket_dir.path());

        Ok(TmpPostgrustFactory {
            socket_dir: Arc::new(socket_dir),
            cache_dir,
            config,
            next_port: AtomicU32::new(5432),
        })
    }
    /// Start a new postgresql instance and return a process guard that will ensure it is cleaned
    /// up when dropped.
    #[instrument(skip(self))]
    pub fn new_instance(&self) -> TmpPostgrustResult<synchronous::ProcessGuard> {
        let data_directory =
            TempDir::new("tmp-postgrust-db").map_err(TmpPostgrustError::CreateCacheDirFailed)?;
        let data_directory_path = data_directory.path();

        set_permissions(
            &data_directory,
            metadata(self.cache_dir.path()).unwrap().permissions(),
        )
        .unwrap();
        synchronous::exec_copy_dir(self.cache_dir.path(), data_directory_path)?;

        if !data_directory_path.join("PG_VERSION").exists() {
            return Err(TmpPostgrustError::EmptyDataDirectory);
        };

        File::create(data_directory_path.join("postgresql.conf"))
            .map_err(TmpPostgrustError::CreateConfigFailed)?
            .write_all(self.config.as_bytes())
            .map_err(TmpPostgrustError::CreateConfigFailed)?;

        let port = self
            .next_port
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let mut postgres_process_handle =
            synchronous::start_postgres_subprocess(data_directory_path, port)?;
        let stdout = postgres_process_handle.stdout.take().unwrap();
        let stderr = postgres_process_handle.stderr.take().unwrap();

        let stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        while let Some(Ok(line)) = stderr_reader.next() {
            debug!("Postgresql: {}", line);
            if line.contains("database system is ready to accept connections") {
                info!("temporary database system is read to accept connections");
                break;
            }
        }
        // TODO: Let users configure these
        let dbname = "demo";
        let dbuser = "demo";
        synchronous::exec_create_user(&self.socket_dir.path(), port, dbname).unwrap();
        synchronous::exec_create_db(&self.socket_dir.path(), port, dbname, dbuser).unwrap();

        Ok(synchronous::ProcessGuard {
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
            connection_string: format!(
                "postgresql://{}@{}:{}/{}?host={}",
                dbuser,
                "localhost",
                port,
                dbname,
                self.socket_dir.path().to_str().unwrap()
            ),
            postgres_process: postgres_process_handle,
            _data_directory: data_directory,
            _socket_dir: Arc::clone(&self.socket_dir),
        })
    }

    /// Start a new postgresql instance and return a process guard that will ensure it is cleaned
    /// up when dropped.
    #[cfg(feature = "tokio-process")]
    #[instrument(skip(self))]
    pub async fn new_instance_async(&self) -> TmpPostgrustResult<asynchronous::ProcessGuard> {
        use std::convert::TryInto;

        use nix::sys::signal::{self, Signal};
        use nix::unistd::Pid;
        use tokio::io::AsyncBufReadExt;
        use tokio::sync::oneshot;
        use tokio::{
            fs::{metadata, set_permissions},
            io::BufReader,
        };

        let process_permit = asynchronous::MAX_CONCURRENT_PROCESSES
            .acquire()
            .await
            .unwrap();

        let data_directory =
            TempDir::new("tmp-postgrust-db").map_err(TmpPostgrustError::CreateCacheDirFailed)?;
        let data_directory_path = data_directory.path();

        set_permissions(
            &data_directory,
            metadata(self.cache_dir.path()).await.unwrap().permissions(),
        )
        .await
        .unwrap();
        asynchronous::exec_copy_dir(self.cache_dir.path(), data_directory_path).await?;

        if !data_directory_path.join("PG_VERSION").exists() {
            return Err(TmpPostgrustError::EmptyDataDirectory);
        };

        File::create(data_directory_path.join("postgresql.conf"))
            .map_err(TmpPostgrustError::CreateConfigFailed)?
            .write_all(self.config.as_bytes())
            .map_err(TmpPostgrustError::CreateConfigFailed)?;

        let port = self
            .next_port
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let mut postgres_process_handle =
            asynchronous::start_postgres_subprocess(data_directory_path, port)?;
        let stdout = postgres_process_handle.stdout.take().unwrap();
        let stderr = postgres_process_handle.stderr.take().unwrap();

        let stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        let (send, recv) = oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::select! {
                _ = postgres_process_handle.wait() => {
                    error!("postgresql exited early");
                }
                _ = recv => {
                    signal::kill(
                        Pid::from_raw(postgres_process_handle.id().unwrap().try_into().unwrap()),
                        Signal::SIGINT,
                    )
                    .unwrap();
                    postgres_process_handle.wait().await.unwrap();
                },
            }
        });

        while let Some(line) = stderr_reader.next_line().await.unwrap() {
            debug!("Postgresql: {}", line);
            if line.contains("database system is ready to accept connections") {
                info!("temporary database system is read to accept connections");
                break;
            }
        }
        // TODO: Let users configure these
        let dbname = "demo";
        let dbuser = "demo";
        asynchronous::exec_create_user(&self.socket_dir.path(), port, dbname)
            .await
            .unwrap();
        asynchronous::exec_create_db(&self.socket_dir.path(), port, dbname, dbuser)
            .await
            .unwrap();

        Ok(asynchronous::ProcessGuard {
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
            connection_string: format!(
                "postgresql://{}@{}:{}/{}?host={}",
                dbuser,
                "localhost",
                port,
                dbname,
                self.socket_dir.path().to_str().unwrap()
            ),
            send_done: Some(send),
            _data_directory: data_directory,
            _socket_dir: Arc::clone(&self.socket_dir),
            _process_permit: process_permit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use test_env_log::test;
    use tokio::sync::OnceCell;
    use tokio_postgres::NoTls;

    #[test(tokio::test)]
    async fn it_works() {
        let factory = TmpPostgrustFactory::try_new().expect("failed to create factory");

        let postgresql_proc = factory
            .new_instance()
            .expect("failed to create a new instance");

        let (client, conn) = tokio_postgres::connect(&postgresql_proc.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                error!("connection error: {}", e);
            }
        });

        client.query("SELECT 1;", &[]).await.unwrap();
    }

    #[test(tokio::test)]
    async fn it_works_async() {
        let factory = TmpPostgrustFactory::try_new_async()
            .await
            .expect("failed to create factory");

        let postgresql_proc = factory
            .new_instance_async()
            .await
            .expect("failed to create a new instance");

        let (client, conn) = tokio_postgres::connect(&postgresql_proc.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                error!("connection error: {}", e);
            }
        });

        client.query("SELECT 1;", &[]).await.unwrap();
    }

    #[test(tokio::test)]
    async fn two_simulatenous_processes() {
        let factory = TmpPostgrustFactory::try_new().expect("failed to create factory");

        let proc1 = factory
            .new_instance()
            .expect("failed to create a new instance");

        let proc2 = factory
            .new_instance()
            .expect("failed to create a new instance");

        let (client1, conn1) = tokio_postgres::connect(&proc1.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn1.await {
                error!("connection error: {}", e);
            }
        });

        let (client2, conn2) = tokio_postgres::connect(&proc2.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn2.await {
                error!("connection error: {}", e);
            }
        });

        client1.query("SELECT 1;", &[]).await.unwrap();
        client2.query("SELECT 1;", &[]).await.unwrap();
    }

    #[test(tokio::test)]
    async fn two_simulatenous_processes_async() {
        let factory = TmpPostgrustFactory::try_new_async()
            .await
            .expect("failed to create factory");

        let proc1 = factory
            .new_instance_async()
            .await
            .expect("failed to create a new instance");

        let proc2 = factory
            .new_instance_async()
            .await
            .expect("failed to create a new instance");

        let (client1, conn1) = tokio_postgres::connect(&proc1.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn1.await {
                error!("connection error: {}", e);
            }
        });

        let (client2, conn2) = tokio_postgres::connect(&proc2.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn2.await {
                error!("connection error: {}", e);
            }
        });

        client1.query("SELECT 1;", &[]).await.unwrap();
        client2.query("SELECT 1;", &[]).await.unwrap();
    }

    static FACTORY: OnceCell<TmpPostgrustFactory> = OnceCell::const_new();

    #[test(tokio::test)]
    async fn static_oncecell() {
        let factory = FACTORY
            .get_or_try_init(TmpPostgrustFactory::try_new_async)
            .await
            .unwrap();
        let proc1 = factory.new_instance_async().await.unwrap();

        let (client1, conn1) = tokio_postgres::connect(&proc1.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn1.await {
                error!("connection error: {}", e);
            }
        });

        let factory = FACTORY
            .get_or_try_init(TmpPostgrustFactory::try_new_async)
            .await
            .unwrap();
        let proc2 = factory.new_instance_async().await.unwrap();

        let (client2, conn2) = tokio_postgres::connect(&proc2.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn2.await {
                error!("connection error: {}", e);
            }
        });

        // Shouldn't be able to do this if they are both the same database.
        client1.execute("CREATE TABLE lock ();", &[]).await.unwrap();
        client2.execute("CREATE TABLE lock ();", &[]).await.unwrap();
    }

    // Test that a OnceCell can be used in two async tests.
    static SHARED_FACTORY: OnceCell<TmpPostgrustFactory> = OnceCell::const_new();

    #[test(tokio::test)]
    async fn static_oncecell_shared_1() {
        let factory = SHARED_FACTORY
            .get_or_try_init(TmpPostgrustFactory::try_new_async)
            .await
            .unwrap();
        let proc = factory.new_instance_async().await.unwrap();

        let (client, conn) = tokio_postgres::connect(&proc.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                error!("connection error: {}", e);
            }
        });

        // Chance to catch concurrent tests or database that have already been used.
        client.execute("CREATE TABLE lock ();", &[]).await.unwrap();
    }

    #[test(tokio::test)]
    async fn static_oncecell_shared_2() {
        let factory = SHARED_FACTORY
            .get_or_try_init(TmpPostgrustFactory::try_new_async)
            .await
            .unwrap();
        let proc = factory.new_instance_async().await.unwrap();

        let (client, conn) = tokio_postgres::connect(&proc.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                error!("connection error: {}", e);
            }
        });

        // Chance to catch concurrent tests or database that have already been used.
        client.execute("CREATE TABLE lock ();", &[]).await.unwrap();
    }

    #[test(tokio::test)]
    async fn default_process_factory_1() {
        let proc = new_default_process_async().await.unwrap();

        let (client, conn) = tokio_postgres::connect(&proc.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                error!("connection error: {}", e);
            }
        });

        // Chance to catch concurrent tests or database that have already been used.
        client.execute("CREATE TABLE lock ();", &[]).await.unwrap();
    }

    #[test(tokio::test)]
    async fn default_process_factory_2() {
        let proc = new_default_process_async().await.unwrap();

        let (client, conn) = tokio_postgres::connect(&proc.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                error!("connection error: {}", e);
            }
        });

        // Chance to catch concurrent tests or database that have already been used.
        client.execute("CREATE TABLE lock ();", &[]).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn default_process_factory_multithread_1() {
        let proc = new_default_process_async().await.unwrap();

        let (client, conn) = tokio_postgres::connect(&proc.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                error!("connection error: {}", e);
            }
        });

        // Chance to catch concurrent tests or database that have already been used.
        client.execute("CREATE TABLE lock ();", &[]).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn default_process_factory_multithread_2() {
        let proc = new_default_process_async().await.unwrap();

        let (client, conn) = tokio_postgres::connect(&proc.connection_string, NoTls)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                error!("connection error: {}", e);
            }
        });

        // Chance to catch concurrent tests or database that have already been used.
        client.execute("CREATE TABLE lock ();", &[]).await.unwrap();
    }
}
