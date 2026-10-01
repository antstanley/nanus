//! The Windows link over real named pipes, including the default descriptor read-back.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use nanus_link::protocol::PROTOCOL_VERSION;
use nanus_link::transport::{Listener, Stream, bind};
use nanus_link::wire::{read_request, write_frame};
use nanus_link::{AgentInfo, Client, Frame, LinkError, Request};
use tokio::io::BufReader;

fn endpoint() -> PathBuf {
    // Tests run concurrently and must not use the service's singleton name.
    let base = nanus_link::paths::attached_endpoint(Path::new("unused"), std::process::id())
        .expect("the current user's SID");
    PathBuf::from(format!("{}-{}", base.display(), uuid::Uuid::new_v4()))
}

fn info(version: u32) -> AgentInfo {
    AgentInfo {
        workspace: String::from("C:\\work"),
        model: String::from("scripted"),
        models: Vec::new(),
        effort: None,
        model_efforts: Vec::new(),
        provider: String::new(),
        plan: String::new(),
        providers: Vec::new(),
        tools: 0,
        version,
    }
}

async fn pair(listener: &mut Listener, endpoint: &Path) -> (Stream, Stream) {
    let (server, client) = tokio::join!(listener.accept(), Stream::connect(endpoint));
    (server.expect("accept"), client.expect("connect"))
}

#[tokio::test]
async fn a_handshake_is_kept_and_a_split_connection_moves_both_directions() {
    let endpoint = endpoint();
    let mut listener = bind(&endpoint).await.expect("bind");
    let (mut server, client) = pair(&mut listener, &endpoint).await;
    let expected = info(PROTOCOL_VERSION);
    write_frame(&mut server, &Frame::Ready(expected.clone()))
        .await
        .expect("handshake");
    let client = Client::open(client).await.expect("open");
    assert_eq!(client.info(), &expected);
    let (mut reader, mut sender) = client.split();
    let pending_read = tokio::spawn(async move { reader.next().await });
    sender
        .send(&Request::Status)
        .await
        .expect("send while a read is pending");
    let (read, mut write) = server.into_split();
    assert_eq!(
        read_request(&mut BufReader::new(read))
            .await
            .expect("request"),
        Some(Request::Status)
    );
    write_frame(&mut write, &Frame::Bye).await.expect("reply");
    let reply = tokio::time::timeout(Duration::from_secs(2), pending_read)
        .await
        .expect("the independent reader wakes")
        .expect("reader task")
        .expect("frame");
    assert_eq!(reply, Some(Frame::Bye));
}

#[tokio::test]
async fn a_refused_attachment_keeps_the_agents_message() {
    let endpoint = endpoint();
    let mut listener = bind(&endpoint).await.expect("bind");
    let (mut server, client) = pair(&mut listener, &endpoint).await;
    write_frame(&mut server, &Frame::Ready(info(PROTOCOL_VERSION)))
        .await
        .expect("ready");
    let mut client = Client::open(client).await.expect("open");
    write_frame(
        &mut server,
        &Frame::Failed {
            message: String::from("no such session"),
        },
    )
    .await
    .expect("refusal");
    assert!(matches!(client.attach("missing").await,
        Err(LinkError::Agent(message)) if message == "no such session"));
}

#[tokio::test]
async fn a_peer_that_hangs_up_before_its_handshake_is_closed() {
    let endpoint = endpoint();
    let mut listener = bind(&endpoint).await.expect("bind");
    let (server, client) = pair(&mut listener, &endpoint).await;
    drop(server);
    assert!(matches!(Client::open(client).await, Err(LinkError::Closed)));
}

#[tokio::test]
async fn a_wrong_handshake_and_another_protocol_version_are_refused() {
    for opening in [
        Frame::Bye,
        Frame::Ready(info(PROTOCOL_VERSION.saturating_add(1))),
    ] {
        let endpoint = endpoint();
        let mut listener = bind(&endpoint).await.expect("bind");
        let (mut server, client) = pair(&mut listener, &endpoint).await;
        write_frame(&mut server, &opening).await.expect("opening");
        let error = Client::open(client).await.expect_err("refused");
        match opening {
            Frame::Bye => assert!(matches!(error, LinkError::Protocol(_))),
            _ => assert!(matches!(error, LinkError::Version { .. })),
        }
    }
}

#[tokio::test]
async fn a_second_owner_is_refused_and_the_name_disappears_with_its_handles() {
    let endpoint = endpoint();
    let mut first = bind(&endpoint).await.expect("first bind");
    assert!(
        bind(&endpoint).await.is_err(),
        "a squatter or second owner is refused"
    );
    let (server, client) = pair(&mut first, &endpoint).await;
    assert!(
        bind(&endpoint).await.is_err(),
        "connected instances retain ownership"
    );
    drop(first);
    assert!(
        bind(&endpoint).await.is_err(),
        "a connection still holds the name"
    );
    drop(server);
    drop(client);
    assert!(
        Stream::connect(&endpoint).await.is_err(),
        "there is no stale pipe"
    );
    assert!(bind(&endpoint).await.is_ok(), "the name can now be reused");
}

#[tokio::test]
async fn the_next_instance_exists_before_an_accepted_stream_is_dispatched() {
    let endpoint = endpoint();
    let mut listener = bind(&endpoint).await.expect("bind");
    let (_server, _client) = pair(&mut listener, &endpoint).await;
    // Connect without calling accept yet: the next instance must already exist.
    let _next = Stream::connect(&endpoint)
        .await
        .expect("the next instance is available");
    let _accepted = listener.accept().await.expect("accept the next client");
}

#[tokio::test]
async fn a_cancelled_accept_keeps_the_listener_usable() {
    let endpoint = endpoint();
    let mut listener = bind(&endpoint).await.expect("bind");
    assert!(
        tokio::time::timeout(Duration::from_millis(10), listener.accept())
            .await
            .is_err()
    );
    let (_server, _client) = pair(&mut listener, &endpoint).await;
}

#[tokio::test]
async fn a_remote_endpoint_is_refused_before_connecting_or_binding() {
    let remote = Path::new(r"\\another-machine\pipe\nanus-agent");
    assert!(bind(remote).await.is_err());
    assert_eq!(
        Stream::connect(remote)
            .await
            .expect_err("remote refused")
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[test]
fn the_computed_names_are_stable_scoped_to_the_sid_and_distinct_by_process() {
    let home = Path::new("unused");
    let sid = nanus_sys_windows::current_user_sid().expect("SID");
    let service = nanus_link::paths::service_endpoint(home).expect("service");
    assert_eq!(
        service,
        PathBuf::from(format!(r"\\.\pipe\nanus-{sid}-agent"))
    );
    assert_eq!(
        service,
        nanus_link::paths::service_endpoint(Path::new("other-home")).expect("stable")
    );
    let first = nanus_link::paths::attached_endpoint(home, 41).expect("first");
    assert_eq!(
        first,
        PathBuf::from(format!(r"\\.\pipe\nanus-{sid}-attach-41"))
    );
    assert_ne!(
        first,
        nanus_link::paths::attached_endpoint(home, 42).expect("second")
    );
    assert_ne!(first, service);
}

#[tokio::test]
async fn the_default_descriptor_grants_write_only_to_the_owner_system_and_administrators() {
    let endpoint = endpoint();
    let mut listener = bind(&endpoint).await.expect("bind");
    // Windows PowerShell uses .NET Framework's safe PipeStream.GetAccessControl API.
    // Read the actual pipe ACL rather than a model of the descriptor or hard-coded SDDL.
    let name = endpoint
        .to_str()
        .expect("pipe name")
        .strip_prefix(r"\\.\pipe\")
        .expect("local");
    let command = tokio::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            include_str!("pipe-acl.ps1"),
        ])
        .env("NANUS_TEST_PIPE", name)
        .env(
            "NANUS_TEST_SID",
            nanus_sys_windows::current_user_sid().expect("SID"),
        )
        .output();
    let (accepted, output) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(listener.accept(), command)
    })
    .await
    .expect("descriptor probe must finish");
    let _accepted = accepted.expect("descriptor reader connects");
    let output = output.expect("PowerShell descriptor read-back");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("descriptor verified"));
}
