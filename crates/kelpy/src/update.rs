//! Updates. At start Kelpy asks GitHub for the latest release and, when it is newer, offers to install it: the
//! download is checked, put in place of the executable in the home, and started instead of this copy.
//! `kelpy update` does the same on request.
//!
//! The release workflow signs each release's `SHA256SUMS` with Ed25519 (`SHA256SUMS.sig`, the 64 bytes of the
//! signature). `SHA256SUMS` also lists `VERSION`, a file holding the release's version, so the signature covers the
//! version too. Kelpy installs an update only when that signature matches a key built into it, `VERSION` is the
//! version it asked for, and the download matches its line in `SHA256SUMS`: whoever could change a release on GitHub
//! still could not change what Kelpy installs, nor pass an older release off as a newer one.

use std::cmp::Ordering;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use kelpy_core::VERSION;
use kelpy_core::config::Config;
use kelpy_core::home::{self, Home};
use ring::signature::{ED25519, UnparsedPublicKey};

use crate::setup::{compare_versions, run_installed, same_file};
use crate::{platform, prompt, runtime};

const RELEASES: &str = "https://github.com/Ryadel/Kelpy/releases";

/// The keys that sign releases, as raw Ed25519 public keys; `scripts/release-key.pem` holds the same key for
/// checking a release by hand. To change key, a release signed with the old one adds the new one here.
const RELEASE_KEYS: &[[u8; 32]] = &[[
    0xa1, 0xdf, 0x18, 0x98, 0xcc, 0x84, 0x0f, 0x4d, 0x25, 0x63, 0x19, 0x01, 0xd4, 0xfb, 0x34, 0x6f,
    0x4f, 0x9c, 0x39, 0x15, 0xb6, 0x0d, 0x02, 0xca, 0xab, 0xdc, 0xd1, 0x7f, 0x3a, 0xde, 0xfd, 0xa9,
]];

/// How long the check at start may take: an offline server starts after this at most.
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// How long downloading and checking an update may take in all, however slowly the server sends.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Where releases are, unless `KELPY_RELEASES_URL` names a copy (to test a release build against a local one; the
/// signature is required all the same).
fn releases() -> String {
    env::var("KELPY_RELEASES_URL").unwrap_or_else(|_| RELEASES.to_owned())
}

/// The release file for this platform, as the release workflow names it.
fn asset_name() -> String {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    format!("kelpy-{}-{}{suffix}", env::consts::OS, env::consts::ARCH)
}

/// The update check at start. Returns the exit code of the updated Kelpy, which ran instead of this copy, or `None`
/// to go on with this one. Never fails: without network or with a broken release, Kelpy starts as it is.
pub async fn at_start(home: &Home, assume_yes: bool) -> Option<ExitCode> {
    let installed = home.root().join(home::executable_name());
    // Only the installed copy updates itself: not a build run from elsewhere, not right after the first run.
    let this_copy = env::current_exe().ok()?;
    if !same_file(&installed, &this_copy) {
        return None;
    }
    if !Config::load(&home.config_path()).is_ok_and(|config| config.updates.check)
        || runtime::is_running(home).unwrap_or(true)
    {
        return None;
    }
    let latest = match latest_version(&releases(), CHECK_TIMEOUT).await {
        Ok(latest) => latest,
        Err(error) => {
            tracing::debug!("update check skipped: {error:#}");
            return None;
        }
    };
    if compare_versions(&latest, VERSION) != Ordering::Greater {
        return None;
    }

    let declined = declined_path(home);
    if !prompt::interactive()
        || assume_yes
        || fs::read_to_string(&declined).is_ok_and(|version| version.trim() == latest)
    {
        println!("Kelpy {latest} is available: run `kelpy update` to install it.");
        return None;
    }
    let question = format!("Kelpy {latest} is available (this is {VERSION}). Update now?");
    if !prompt::confirm(&question, true).unwrap_or(false) {
        // Asked once per version; the line above reminds at the next starts.
        let _ = fs::write(&declined, &latest);
        return None;
    }
    // Kelpy may have started in another terminal while the question waited.
    let Ok(Some(hold)) = runtime::hold(home) else {
        println!("Kelpy started meanwhile: run `kelpy update` once it stops.");
        return None;
    };
    let installed_now = install(
        &installed,
        &home.data_dir(),
        &latest,
        &releases(),
        RELEASE_KEYS,
        INSTALL_TIMEOUT,
    )
    .await;
    // The updated Kelpy takes the lock itself.
    drop(hold);
    if let Err(error) = installed_now {
        eprintln!("Update failed: {error:#}. Starting Kelpy {VERSION}.");
        return None;
    }
    println!("✓ Updated Kelpy in {} to {latest}", home.root().display());
    Some(run_installed(&installed).await.unwrap_or_else(|error| {
        eprintln!("Error: {error:#}");
        ExitCode::FAILURE
    }))
}

/// `kelpy update`: installs the latest release if it is newer, asking first unless `--yes`.
pub async fn command(home: &Home, assume_yes: bool) -> Result<ExitCode> {
    let installed = home.root().join(home::executable_name());
    if !installed.is_file() {
        bail!(
            "{} has no Kelpy executable to update.",
            home.root().display()
        );
    }
    let latest = latest_version(&releases(), Duration::from_secs(20))
        .await
        .context("cannot find the latest release")?;
    if compare_versions(&latest, VERSION) != Ordering::Greater {
        println!("Kelpy {VERSION} is the latest version.");
        return Ok(ExitCode::SUCCESS);
    }
    let running = || {
        anyhow::anyhow!(
            "Kelpy is running from {}. Stop it first (Ctrl+C in its window), then run this again.",
            home.root().display()
        )
    };
    if runtime::is_running(home)? {
        return Err(running());
    }
    if !assume_yes {
        if !prompt::interactive() {
            bail!(
                "Kelpy {latest} is available. Run `kelpy update --yes` to install it without questions."
            );
        }
        let question = format!("Install Kelpy {latest} (this is {VERSION})?");
        if !prompt::confirm(&question, true)? {
            return Ok(ExitCode::SUCCESS);
        }
    }
    // Held until the new executable is in place, so that no Kelpy starts meanwhile.
    let _hold = runtime::hold(home)?.ok_or_else(running)?;
    install(
        &installed,
        &home.data_dir(),
        &latest,
        &releases(),
        RELEASE_KEYS,
        INSTALL_TIMEOUT,
    )
    .await?;
    let _ = fs::remove_file(declined_path(home));
    println!("✓ Updated Kelpy in {} to {latest}", home.root().display());
    Ok(ExitCode::SUCCESS)
}

/// The version a declined update offered, so that it is not asked again.
fn declined_path(home: &Home) -> PathBuf {
    home.data_dir().join("update-declined.txt")
}

/// The latest release's version, without the tag's `v`. `releases/latest` redirects to the latest release's tag
/// page, which names it: one request, and no API rate limit.
async fn latest_version(releases: &str, timeout: Duration) -> Result<String> {
    let client = kelpy_core::providers::http_client(|builder| {
        builder
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
    })?;
    let url = format!("{releases}/latest");
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .with_context(|| format!("{url} did not name a release"))?;
    tag_version(location).with_context(|| format!("{url} did not name a release: {location}"))
}

/// `…/releases/tag/v1.2.3` → `1.2.3`.
fn tag_version(location: &str) -> Option<String> {
    let (_, tag) = location.rsplit_once("/tag/")?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    let numbers: Vec<&str> = version.split(['-', '+']).next()?.split('.').collect();
    let valid = numbers.len() == 3
        && numbers
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    valid.then(|| version.to_owned())
}

/// Downloads release `version`, checks it, and writes it over `installed`, staged in `scratch` (see
/// `platform::write_executable`). Downloading and checking must end within `timeout`.
async fn install(
    installed: &Path,
    scratch: &Path,
    version: &str,
    releases: &str,
    keys: &[[u8; 32]],
    timeout: Duration,
) -> Result<()> {
    let executable = tokio::time::timeout(timeout, download_checked(version, releases, keys))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "the download of release {version} did not end within {} seconds",
                timeout.as_secs_f32()
            )
        })??;
    platform::write_executable(&executable, installed, scratch)
        .with_context(|| format!("cannot update {}", installed.display()))
}

/// This platform's executable of release `version`, once its signature, version and checksum are checked.
async fn download_checked(version: &str, releases: &str, keys: &[[u8; 32]]) -> Result<Vec<u8>> {
    let client = kelpy_core::providers::http_client(|builder| {
        builder
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
    })?;
    let asset = asset_name();
    let base = format!("{releases}/download/v{version}");
    let sums = download(&client, &format!("{base}/SHA256SUMS"), 64 << 10).await?;
    let signature = download(&client, &format!("{base}/SHA256SUMS.sig"), 1 << 10).await?;
    if !signed(&sums, &signature, keys) {
        bail!("the signature of release {version} is not valid, so it was not installed");
    }
    let sums = String::from_utf8(sums).context("SHA256SUMS is not text")?;
    if listed_digest(&sums, "VERSION") != Some(version_digest(version)) {
        bail!("the signed files are not those of release {version}, so it was not installed");
    }
    let expected = listed_digest(&sums, &asset)
        .with_context(|| format!("{asset} is not listed in SHA256SUMS"))?;

    println!("Downloading {asset}");
    let executable = download(&client, &format!("{base}/{asset}"), 512 << 20).await?;
    let actual = hex(ring::digest::digest(&ring::digest::SHA256, &executable).as_ref());
    if actual != expected {
        bail!("checksum mismatch for {asset} (expected {expected}, got {actual})");
    }
    Ok(executable)
}

/// The digest `SHA256SUMS` lists for `VERSION` in release `version`: the file holds the version and a newline.
fn version_digest(version: &str) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, format!("{version}\n").as_bytes()).as_ref())
}

async fn download(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("cannot download {url}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        bail!("{url} is larger than a release file can be");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("cannot download {url}"))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > limit {
            bail!("{url} is larger than a release file can be");
        }
    }
    Ok(body)
}

fn signed(message: &[u8], signature: &[u8], keys: &[[u8; 32]]) -> bool {
    keys.iter().any(|key| {
        UnparsedPublicKey::new(&ED25519, key)
            .verify(message, signature)
            .is_ok()
    })
}

/// The digest `SHA256SUMS` lists for `asset`, in lowercase: `<digest>  <name>` or `<digest> *<name>` lines.
fn listed_digest(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (digest, name) = line.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start();
        (name.strip_prefix('*').unwrap_or(name) == asset).then(|| digest.to_ascii_lowercase())
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn the_built_in_key_is_the_published_one() {
        let pem = include_str!("../../../scripts/release-key.pem");
        let body: String = pem
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(body)
            .unwrap();
        // An Ed25519 SubjectPublicKeyInfo is a fixed 12-byte header and the 32 bytes of the key.
        assert_eq!(der.len(), 44);
        assert_eq!(der[12..], RELEASE_KEYS[0]);
    }

    #[test]
    fn the_asset_name_follows_the_release_names() {
        let name = asset_name();
        if cfg!(all(windows, target_arch = "x86_64")) {
            assert_eq!(name, "kelpy-windows-x86_64.exe");
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(name, "kelpy-linux-x86_64");
        }
    }

    #[test]
    fn the_version_comes_from_the_tag_page() {
        let location = "https://github.com/Ryadel/Kelpy/releases/tag/v0.2.0";
        assert_eq!(tag_version(location).as_deref(), Some("0.2.0"));
        assert_eq!(
            tag_version("/releases/tag/v1.10.3-rc.1").as_deref(),
            Some("1.10.3-rc.1")
        );
        // Without any release, GitHub sends `latest` to the list of releases.
        assert_eq!(
            tag_version("https://github.com/Ryadel/Kelpy/releases"),
            None
        );
        assert_eq!(tag_version("/releases/tag/nightly"), None);
        assert_eq!(tag_version("/releases/tag/v1.2"), None);
    }

    #[test]
    fn digests_are_found_by_exact_name() {
        let sums = "AB12  kelpy-linux-x86_64\ncd34 *kelpy-windows-x86_64.exe\nef56  kelpy-linux-x86_64.old\n";
        assert_eq!(
            listed_digest(sums, "kelpy-linux-x86_64").as_deref(),
            Some("ab12")
        );
        assert_eq!(
            listed_digest(sums, "kelpy-windows-x86_64.exe").as_deref(),
            Some("cd34")
        );
        assert_eq!(listed_digest(sums, "kelpy-linux-aarch64"), None);
    }

    struct Release {
        keys: Vec<[u8; 32]>,
        sums: String,
        signature: Vec<u8>,
        executable: Vec<u8>,
        /// Sends the executable a byte at a time, with this pause before each.
        drip: Option<Duration>,
    }

    /// Release 9.9.9 of `executable`, as `release_of` makes it.
    fn release(executable: &[u8]) -> (Release, Ed25519KeyPair) {
        release_of("9.9.9", executable)
    }

    /// Release `version` of `executable`, with its checksums signed by a new key, which `keys` trusts.
    fn release_of(version: &str, executable: &[u8]) -> (Release, Ed25519KeyPair) {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let digest = hex(ring::digest::digest(&ring::digest::SHA256, executable).as_ref());
        let sums = format!(
            "{}  VERSION\n{digest}  {}\n",
            version_digest(version),
            asset_name()
        );
        let signature = pair.sign(sums.as_bytes()).as_ref().to_vec();
        let key: [u8; 32] = pair.public_key().as_ref().try_into().unwrap();
        let release = Release {
            keys: vec![key],
            sums,
            signature,
            executable: executable.to_vec(),
            drip: None,
        };
        (release, pair)
    }

    /// Serves `release` as GitHub does: `latest` redirects to the tag of version 9.9.9, files under `download/`.
    async fn serve(release: Release) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/releases", listener.local_addr().unwrap());
        let release = std::sync::Arc::new(release);
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let release = release.clone();
                tokio::spawn(async move {
                    let mut request = vec![0; 4096];
                    let read = socket.read(&mut request).await.unwrap();
                    let line = String::from_utf8_lossy(&request[..read]).to_string();
                    let path = line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_owned();
                    let file = path.rsplit('/').next().unwrap_or_default().to_owned();
                    let drip = (file == asset_name()).then_some(release.drip).flatten();
                    let (head, body): (String, Vec<u8>) = if path.ends_with("/latest") {
                        (
                            "302 Found\r\nlocation: /releases/tag/v9.9.9".into(),
                            Vec::new(),
                        )
                    } else if file == "SHA256SUMS" {
                        ("200 OK".into(), release.sums.clone().into_bytes())
                    } else if file == "SHA256SUMS.sig" {
                        ("200 OK".into(), release.signature.clone())
                    } else if file == asset_name() {
                        ("200 OK".into(), release.executable.clone())
                    } else {
                        ("404 Not Found".into(), Vec::new())
                    };
                    let head = format!(
                        "HTTP/1.1 {head}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    match drip {
                        None => {
                            let _ = socket.write_all(&body).await;
                        }
                        Some(pause) => {
                            for byte in body {
                                tokio::time::sleep(pause).await;
                                if socket.write_all(&[byte]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        url
    }

    #[tokio::test]
    async fn the_latest_version_is_read_from_the_redirect() {
        let (release, _) = release(b"new");
        let url = serve(release).await;
        assert_eq!(latest_version(&url, CHECK_TIMEOUT).await.unwrap(), "9.9.9");
    }

    #[tokio::test]
    async fn a_signed_release_replaces_the_executable() {
        let (release, _) = release(b"the new kelpy");
        let keys = release.keys.clone();
        let url = serve(release).await;
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join(home::executable_name());
        fs::write(&installed, b"the old kelpy").unwrap();

        install(
            &installed,
            dir.path(),
            "9.9.9",
            &url,
            &keys,
            INSTALL_TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(fs::read(&installed).unwrap(), b"the new kelpy");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&installed).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[tokio::test]
    async fn a_release_signed_by_another_key_is_refused() {
        let (release, _) = release(b"the new kelpy");
        let (_, stranger) = self::release(b"");
        let trusted: [u8; 32] = stranger.public_key().as_ref().try_into().unwrap();
        let url = serve(release).await;
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join(home::executable_name());
        fs::write(&installed, b"the old kelpy").unwrap();

        let error = install(
            &installed,
            dir.path(),
            "9.9.9",
            &url,
            &[trusted],
            INSTALL_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("signature"), "{error:#}");
        assert_eq!(fs::read(&installed).unwrap(), b"the old kelpy");
    }

    #[tokio::test]
    async fn an_executable_that_does_not_match_its_checksum_is_refused() {
        let (mut release, _) = release(b"the new kelpy");
        release.executable = b"something else".to_vec();
        let keys = release.keys.clone();
        let url = serve(release).await;
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join(home::executable_name());
        fs::write(&installed, b"the old kelpy").unwrap();

        let error = install(
            &installed,
            dir.path(),
            "9.9.9",
            &url,
            &keys,
            INSTALL_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"), "{error:#}");
        assert_eq!(fs::read(&installed).unwrap(), b"the old kelpy");
    }

    #[tokio::test]
    async fn an_older_release_under_a_newer_tag_is_refused() {
        // Release 9.9.8, signed and intact, served as 9.9.9.
        let (release, _) = release_of("9.9.8", b"the older kelpy");
        let keys = release.keys.clone();
        let url = serve(release).await;
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join(home::executable_name());
        fs::write(&installed, b"the current kelpy").unwrap();

        let error = install(
            &installed,
            dir.path(),
            "9.9.9",
            &url,
            &keys,
            INSTALL_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("release 9.9.9"), "{error:#}");
        assert_eq!(fs::read(&installed).unwrap(), b"the current kelpy");
    }

    #[tokio::test]
    async fn a_download_that_never_ends_is_stopped() {
        // Each byte comes well within any read timeout, the whole file much later than the limit.
        let (mut release, _) = release(b"the new kelpy");
        release.drip = Some(Duration::from_millis(100));
        let keys = release.keys.clone();
        let url = serve(release).await;
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join(home::executable_name());
        fs::write(&installed, b"the old kelpy").unwrap();

        let error = install(
            &installed,
            dir.path(),
            "9.9.9",
            &url,
            &keys,
            Duration::from_millis(500),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("did not end"), "{error:#}");
        assert_eq!(fs::read(&installed).unwrap(), b"the old kelpy");
    }
}
