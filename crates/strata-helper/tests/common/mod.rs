//! Shared test utilities: synthetic NTFS images, an in-process helper on a
//! real named pipe, and self-deleting scratch directories.
//!
//! Every file a test creates lives under a dedicated `strata-helper-tests`
//! directory that the test created and removes on drop.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use strata_helper::client::{ClientConfig, HelperClient};
use strata_helper::ops::Shared;
use strata_helper::server::{ExitReason, HelperConfig};
use strata_helper::source::Volumes;
use strata_helper::verify::{LaunchedClientVerifier, trust_policy};
use strata_ipc::pipe::ServerConfig;
use strata_ipc::rate::RateLimit;
use strata_ipc::security::{PeerVerifier, session_pipe_name};
use strata_ntfs::test_image::{Geometry, ImageBuilder, ROOT, RecordBuilder};

/// A unique, self-deleting directory under `strata-helper-tests`.
#[derive(Debug)]
pub struct TestDir {
    pub path: PathBuf,
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn base() -> PathBuf {
    // D: has room and is not the system drive; fall back to %TEMP%.
    if Path::new(r"D:\").exists() {
        PathBuf::from(r"D:\strata-helper-tests")
    } else {
        std::env::temp_dir().join("strata-helper-tests")
    }
}

impl TestDir {
    pub fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let path = base().join(format!("{tag}-{}-{nanos:x}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    pub fn file(&self, name: &str, contents: &[u8]) -> PathBuf {
        let p = self.path.join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        // NOTE: tests create only plain files and directories here (no
        // links), so a recursive delete cannot leave this directory.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// An image with system files plus `files` plain files under the root
/// (records 16..16+files).
pub fn image_with_files(files: u64) -> Vec<u8> {
    let mut b = ImageBuilder::new(Geometry::default())
        .with_system_files()
        .min_records(16 + files);
    for n in 16..16 + files {
        b.insert(
            n,
            RecordBuilder::file(1, ROOT, &format!("file-{n}.bin")).data("", b"x"),
        );
    }
    b.finish()
}

/// Writes `image` into `dir` and returns its path.
pub fn write_image(dir: &TestDir, image: &[u8]) -> PathBuf {
    dir.file("volume.img", image)
}

/// This test binary as the trusted peer (it is both client and server).
pub fn self_policy() -> Arc<dyn PeerVerifier> {
    Arc::new(trust_policy(std::env::current_exe().unwrap()).unwrap())
}

/// An in-process helper serving one pipe on a background thread.
pub struct TestHelper {
    pub pipe: String,
    pub stop: Arc<AtomicBool>,
    pub shared: Arc<Shared>,
    done: mpsc::Receiver<Result<ExitReason, String>>,
    thread: Option<JoinHandle<()>>,
}

/// Knobs for [`TestHelper::start`].
pub struct HelperSetup {
    pub image: Option<PathBuf>,
    pub exit_on_disconnect: bool,
    pub rate_limit: RateLimit,
    pub client_pid: u32,
    pub chunk_bytes: usize,
}

impl Default for HelperSetup {
    fn default() -> Self {
        Self {
            image: None,
            exit_on_disconnect: false,
            rate_limit: RateLimit::default(),
            client_pid: std::process::id(),
            chunk_bytes: 64 * 1024,
        }
    }
}

impl TestHelper {
    pub fn start(setup: HelperSetup) -> Self {
        let sid = strata_win::process::current_user().unwrap().sid;
        let pipe = session_pipe_name(&sid).unwrap();
        let verifier = Arc::new(LaunchedClientVerifier::new(self_policy(), setup.client_pid));
        let mut server = ServerConfig::new(&pipe, sid, verifier);
        server.rate_limit = setup.rate_limit;
        server.capabilities = strata_helper::run::capabilities(false, setup.image.is_some());
        let mut config = HelperConfig::on_demand(server);
        config.exit_on_disconnect = setup.exit_on_disconnect;
        config.accept_timeout = Some(Duration::from_secs(60));
        let stop = Arc::clone(&config.stop);
        let volumes = match setup.image {
            Some(p) => Volumes::with_image(p),
            None => Volumes::raw(),
        };
        let mut shared = Shared::new(volumes);
        shared.scan_chunk_bytes = setup.chunk_bytes;
        let shared = Arc::new(shared);
        let (tx, done) = mpsc::channel();
        let (ready_tx, ready) = mpsc::channel();
        let s = Arc::clone(&shared);
        let thread = std::thread::spawn(move || {
            let mut srv = match strata_ipc::pipe::PipeServer::create(config.server.clone()) {
                Ok(p) => p,
                Err(e) => {
                    let _ = ready_tx.send(());
                    let _ = tx.send(Err(e.to_string()));
                    return;
                }
            };
            let _ = ready_tx.send(());
            let r = strata_helper::server::serve_on(&mut srv, &config, &s);
            let _ = tx.send(r.map_err(|e| e.to_string()));
        });
        ready.recv().unwrap();
        Self {
            pipe,
            stop,
            shared,
            done,
            thread: Some(thread),
        }
    }

    pub fn client_config(&self) -> ClientConfig {
        let mut c = ClientConfig::pipe(&self.pipe);
        c.options.connect_timeout = Duration::from_secs(10);
        c.options.server_verifier = Some(self_policy());
        c.request_timeout = Duration::from_secs(30);
        c
    }

    pub fn connect(&self) -> HelperClient {
        HelperClient::connect(self.client_config()).unwrap()
    }

    /// Waits for the server loop to return.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<Result<ExitReason, String>> {
        let r = self.done.recv_timeout(timeout).ok();
        if r.is_some()
            && let Some(t) = self.thread.take()
        {
            t.join().unwrap();
        }
        r
    }
}

impl Drop for TestHelper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
