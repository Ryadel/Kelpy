//! Local models with Ollama: whether it answers, the models installed, the models suggested for this computer's
//! memory, and downloads that go on in the background.
//!
//! A download belongs to Ollama, not to Kelpy: Kelpy starts it and follows its progress. When Kelpy stops, it stops
//! following and the connection closes; Ollama keeps what it downloaded, and the next download of the same model
//! resumes from there.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kelpy_common::lock;
use kelpy_providers::ollama::LocalModel;
use kelpy_providers::{Kind, Provider};
use serde::Serialize;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// A model worth downloading, for a computer with at least `memory` bytes of RAM.
#[derive(Debug, Serialize)]
pub struct Suggestion {
    pub name: &'static str,
    /// Bytes to download.
    pub size: u64,
    /// Bytes of RAM the computer should have.
    pub memory: u64,
    pub description: &'static str,
}

const GB: u64 = 1_000_000_000;

/// Models that use tools, smallest first, with the RAM each needs for a 64,000-token context. Checked 2026-10-31.
const SUGGESTIONS: &[Suggestion] = &[
    Suggestion {
        name: "qwen3.5:4b",
        size: 3_300_000_000,
        memory: 8 * GB,
        description: "Qwen 3.5, 4 billion parameters. The smallest that uses tools; suited to simple tasks.",
    },
    Suggestion {
        name: "qwen3.5:9b",
        size: 6_600_000_000,
        memory: 16 * GB,
        description: "Qwen 3.5, 9 billion parameters.",
    },
    Suggestion {
        name: "qwen3.8:27b",
        size: 18 * GB,
        memory: 32 * GB,
        description: "Qwen 3.8, 27 billion parameters. Made for long multi-step tasks.",
    },
    Suggestion {
        name: "gpt-oss:120b",
        size: 65 * GB,
        memory: 96 * GB,
        description: "OpenAI's open model, 120 billion parameters.",
    },
];

/// What the setup shows about Ollama at one address.
#[derive(Debug, Serialize)]
pub struct OllamaStatus {
    pub base_url: String,
    /// Whether Ollama answered.
    pub running: bool,
    /// Why it did not, in the provider's words.
    pub error: Option<String>,
    /// Whether the `ollama` program is installed on this computer. Only checked when it does not answer at a
    /// local address.
    pub installed: bool,
    pub models: Vec<LocalModel>,
    /// This computer's RAM, in bytes, if known.
    pub memory: Option<u64>,
    /// The models that fit this computer's memory, smallest first; all of them when the memory is unknown or
    /// Ollama runs elsewhere.
    pub suggestions: Vec<&'static Suggestion>,
    /// The downloads under way, at any address.
    pub downloads: Vec<Download>,
}

/// A download's progress.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Download {
    pub base_url: String,
    pub model: String,
    /// Ollama's status, such as `pulling manifest`.
    pub status: String,
    pub completed: u64,
    pub total: u64,
    pub done: bool,
    /// Why it failed, once it has.
    pub error: Option<String>,
}

/// Kelpy is stopping: a download cannot start.
#[derive(Debug, thiserror::Error)]
#[error("Kelpy is stopping: no new downloads can start")]
pub struct Stopping;

pub struct Ollama {
    provider: Provider,
    downloads: Mutex<Downloads>,
    /// Where downloads run, so that stopping Kelpy can wait for them.
    tasks: TaskTracker,
    /// Ends the downloads under way: Kelpy is stopping.
    stop: CancellationToken,
}

#[derive(Default)]
struct Downloads {
    /// By address and model: asking again for one follows it instead of starting another.
    under_way: HashMap<(String, String), watch::Receiver<Download>>,
    /// No new download starts: Kelpy is stopping.
    closed: bool,
}

impl Ollama {
    /// Downloads run in `tasks`.
    pub fn new(provider: Provider, tasks: TaskTracker) -> Self {
        Self {
            provider,
            downloads: Mutex::new(Downloads::default()),
            tasks,
            stop: CancellationToken::new(),
        }
    }

    /// Ollama's state at `base_url`, or at its default address.
    pub async fn status(&self, base_url: Option<&str>) -> OllamaStatus {
        let base_url = base_url
            .map(|url| url.trim().trim_end_matches('/'))
            .filter(|url| !url.is_empty())
            .or(Kind::Ollama.default_base_url())
            .unwrap_or_default()
            .to_owned();
        let local = is_local(&base_url);
        let (models, error) = match self.provider.ollama_models(&base_url).await {
            Ok(models) => (models, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        let memory = local.then(total_memory).flatten();
        OllamaStatus {
            running: error.is_none(),
            installed: error.is_none() || (local && is_installed()),
            error,
            models,
            memory,
            suggestions: SUGGESTIONS
                .iter()
                .filter(|suggestion| memory.is_none_or(|memory| suggestion.memory <= memory))
                .collect(),
            downloads: lock(&self.downloads)
                .under_way
                .values()
                .map(|download| download.borrow().clone())
                .collect(),
            base_url,
        }
    }

    /// Downloads `model` into the Ollama at `base_url`, or follows the download already under way. `guard` is kept
    /// until the download ends, for example to keep Kelpy from stopping for inactivity. Once [`Ollama::close`] was
    /// called, only following is possible.
    pub fn download(
        self: &Arc<Self>,
        base_url: &str,
        model: &str,
        guard: impl Send + 'static,
    ) -> Result<watch::Receiver<Download>, Stopping> {
        let key = (base_url.to_owned(), model.trim().to_owned());
        // Checked and recorded under one lock, so a download cannot start after `close` returned.
        let mut downloads = lock(&self.downloads);
        if let Some(download) = downloads.under_way.get(&key) {
            return Ok(download.clone());
        }
        if downloads.closed {
            return Err(Stopping);
        }
        let (progress, follower) = watch::channel(Download {
            base_url: key.0.clone(),
            model: key.1.clone(),
            status: "starting".to_owned(),
            ..Download::default()
        });
        downloads.under_way.insert(key.clone(), follower.clone());
        let ollama = Arc::clone(self);
        self.tasks.spawn(async move {
            let _guard = guard;
            let (base_url, model) = &key;
            let pull = ollama.provider.ollama_pull(base_url, model, |step| {
                progress.send_modify(|download| {
                    download.status = step.status;
                    download.completed = step.completed;
                    download.total = step.total;
                });
            });
            let error = tokio::select! {
                result = pull => result.err().map(|error| error.to_string()),
                () = ollama.stop.cancelled() => Some("Kelpy stopped; the next download resumes it".to_owned()),
            };
            // Off the list first, so a status read after the end does not show it as under way.
            lock(&ollama.downloads).under_way.remove(&key);
            progress.send_modify(|download| {
                download.done = true;
                download.error = error;
            });
        });
        Ok(follower)
    }

    /// Refuses new downloads from now on and stops those under way.
    pub fn close(&self) {
        lock(&self.downloads).closed = true;
        self.stop.cancel();
    }
}

/// Whether `base_url` points at this computer.
fn is_local(base_url: &str) -> bool {
    let authority = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(ipv6) => ipv6.split(']').next(),
        None => authority.split(':').next(),
    };
    matches!(host, Some("localhost" | "127.0.0.1" | "::1"))
}

/// Whether the `ollama` program is on this computer: on the `PATH`, or where its installers put it.
fn is_installed() -> bool {
    let program = if cfg!(windows) {
        "ollama.exe"
    } else {
        "ollama"
    };
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()));
    let known = [
        dirs::data_local_dir().map(|dir| dir.join("Programs").join("Ollama").join(program)),
        Some("/Applications/Ollama.app".into()),
        Some("/usr/local/bin/ollama".into()),
    ];
    on_path || known.into_iter().flatten().any(|path| path.exists())
}

/// This computer's RAM, in bytes.
fn total_memory() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

        let mut status = MEMORYSTATUSEX {
            dwLength: size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY: `status` is a valid MEMORYSTATUSEX with its length set, as the call requires.
        (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status.ullTotalPhys)
    }
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
        let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kib * 1024)
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&output.stdout).trim().parse().ok()
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kelpy_test_support::FakeServer;
    use serde_json::json;

    #[test]
    fn local_addresses_are_recognised() {
        assert!(is_local("http://localhost:11434/v1"));
        assert!(is_local("http://127.0.0.1:11434"));
        assert!(is_local("http://[::1]:11434/v1"));
        assert!(!is_local("http://10.0.0.5:11434/v1"));
        assert!(!is_local("http://localhost.example.com/v1"));
    }

    #[test]
    fn suggestions_are_ordered_and_this_computer_has_memory() {
        assert!(
            SUGGESTIONS
                .windows(2)
                .all(|pair| pair[0].memory <= pair[1].memory)
        );
        assert!(total_memory().is_some_and(|memory| memory > GB));
    }

    #[tokio::test]
    async fn a_download_is_followed_until_it_ends_and_never_started_twice() {
        let lines = [
            json!({ "status": "pulling abc", "total": 100, "completed": 100 }),
            json!({ "status": "success" }),
        ];
        let body = lines.map(|line| format!("{line}\n")).concat();
        let server = FakeServer::start(vec![("POST /api/pull", "200 OK", body)]).await;
        let gate = server.hold("POST /api/pull");
        let ollama = Arc::new(Ollama::new(Provider::new().unwrap(), TaskTracker::new()));

        let mut first = ollama.download(&server.url, "qwen3:8b", ()).unwrap();
        let _second = ollama.download(&server.url, "qwen3:8b", ()).unwrap();
        server.received("/api/pull").await;
        let status = ollama.status(Some(&server.url)).await;
        assert_eq!(status.downloads.len(), 1);

        gate.open();
        let done = first
            .wait_for(|download| download.done)
            .await
            .unwrap()
            .clone();
        assert_eq!(done.error, None);
        assert_eq!(done.status, "success");
        assert_eq!(
            server
                .requests()
                .iter()
                .filter(|r| r.contains("/api/pull"))
                .count(),
            1
        );
        assert!(ollama.status(Some(&server.url)).await.downloads.is_empty());
    }

    #[tokio::test]
    async fn an_ollama_that_does_not_answer_is_reported() {
        let ollama = Ollama::new(Provider::new().unwrap(), TaskTracker::new());
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        // Nothing listens there any more, and the address is not this computer's by name.
        let status = ollama
            .status(Some(&format!("http://127.0.0.2:{port}/v1")))
            .await;
        assert!(!status.running);
        assert!(!status.installed, "never checked for another computer");
        assert!(status.error.is_some());
        assert_eq!(status.memory, None);
    }
}
