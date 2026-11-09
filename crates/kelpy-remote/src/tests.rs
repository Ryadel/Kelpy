use kelpy_core::events::Events;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};

use super::*;

struct Client;

impl russh::client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, _: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// A server allowing `key` without asking, with its files in `folder`.
async fn server(folder: &tempfile::TempDir, key: &PrivateKey) -> Remote {
    let events = Events::new();
    let connections = Arc::new(Connections::new(
        folder.path().join("remote.log"),
        events.clone(),
    ));
    connections.allow_keys(vec![
        key.public_key().fingerprint(HashAlg::Sha256).to_string(),
    ]);
    start(RemoteOptions {
        port: 0,
        host_key: folder.path().join("host_key"),
        files: Arc::new(Files::new(folder.path().join("files"), events)),
        web_port: 9,
        api_url: "http://127.0.0.1:9".into(),
        token: "token".into(),
        banner: Default::default(),
        connections,
    })
    .await
    .unwrap()
}

fn key() -> PrivateKey {
    PrivateKey::from(Ed25519Keypair::from_seed(&[7; 32]))
}

/// A client signed in with `key`.
async fn signed_in(port: u16, key: &PrivateKey) -> russh::client::Handle<Client> {
    let config = Arc::new(russh::client::Config::default());
    let mut client = russh::client::connect(config, (Ipv4Addr::LOCALHOST, port), Client)
        .await
        .unwrap();
    let signed = client
        .authenticate_publickey(
            "admin",
            PrivateKeyWithHashAlg::new(Arc::new(key.clone()), None),
        )
        .await
        .unwrap();
    assert!(signed.success());
    client
}

#[tokio::test]
async fn stopping_the_server_ends_the_sessions_it_allowed() {
    let folder = tempfile::tempdir().unwrap();
    let key = key();
    let remote = server(&folder, &key).await;
    let client = signed_in(remote.port, &key).await;
    client.channel_open_session().await.unwrap();

    assert!(remote.stop(Duration::from_secs(5)).await);
    assert!(client.channel_open_session().await.is_err());
}

/// Whether the server agreed to the last request on `channel`.
async fn agreed(channel: &mut russh::Channel<russh::client::Msg>) -> bool {
    loop {
        match channel.wait().await {
            Some(russh::ChannelMsg::Success) => return true,
            Some(russh::ChannelMsg::Failure) | None => return false,
            Some(_) => {}
        }
    }
}

#[tokio::test]
async fn a_session_has_a_bounded_number_of_channels_each_running_one_thing() {
    let folder = tempfile::tempdir().unwrap();
    let key = key();
    let remote = server(&folder, &key).await;
    let client = signed_in(remote.port, &key).await;
    let mut channels = Vec::new();
    for _ in 0..MAX_CHANNELS {
        channels.push(client.channel_open_session().await.unwrap());
    }
    assert!(client.channel_open_session().await.is_err());

    // One thing per channel.
    channels[0].request_subsystem(true, "sftp").await.unwrap();
    assert!(agreed(&mut channels[0]).await);
    channels[0].request_subsystem(true, "sftp").await.unwrap();
    assert!(!agreed(&mut channels[0]).await);

    // A channel closed makes room for another.
    channels.pop().unwrap().close().await.unwrap();
    client.channel_open_session().await.unwrap();
    assert!(remote.stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn clients_that_say_nothing_do_not_keep_others_out() {
    let folder = tempfile::tempdir().unwrap();
    let key = key();
    let remote = server(&folder, &key).await;
    let mut silent = Vec::new();
    for _ in 0..MAX_WAITING {
        silent.push(
            TcpStream::connect((Ipv4Addr::LOCALHOST, remote.port))
                .await
                .unwrap(),
        );
    }
    tokio::time::sleep(HELLO_WAIT * 2).await;
    let client = signed_in(remote.port, &key).await;
    client.channel_open_session().await.unwrap();
    assert!(remote.stop(Duration::from_secs(5)).await);
}

#[test]
fn output_a_client_does_not_read_is_bounded() {
    let output = Arc::new(Output::default());
    let mut frames = Frames {
        output: Arc::clone(&output),
        pending: Vec::new(),
    };
    let frame = vec![b'x'; 64 * 1024];
    let mut drawn = 0;
    let failed = loop {
        frames.write_all(&frame).unwrap();
        match frames.flush() {
            Ok(()) => drawn += 1,
            Err(error) => break error,
        }
    };
    assert_eq!(failed.kind(), io::ErrorKind::TimedOut);
    assert_eq!(drawn, MAX_BEHIND / frame.len());
    assert!(lock(&output.backlog).bytes.len() <= MAX_BEHIND);
}

#[test]
fn the_host_key_is_made_once_and_kept() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("ssh_host_ed25519_key");
    let made = host_key(&path).unwrap();
    let read = host_key(&path).unwrap();
    assert_eq!(
        made.public_key().fingerprint(HashAlg::Sha256),
        read.public_key().fingerprint(HashAlg::Sha256)
    );
    std::fs::write(&path, "not a key").unwrap();
    assert!(matches!(host_key(&path), Err(RemoteError::HostKey(..))));
}

#[test]
fn terminal_sizes_fit() {
    assert_eq!(size(120, 40), (120, 40));
    assert_eq!(size(0, 100_000), (1, MAX_ROWS));
    assert_eq!(size(65_535, 65_535), (MAX_COLUMNS, MAX_ROWS));
}
