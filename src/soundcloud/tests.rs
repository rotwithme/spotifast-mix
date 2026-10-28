use super::*;
use std::{path::PathBuf, sync::atomic::AtomicUsize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const USER: &str = r#"{"urn":"soundcloud:users:7","username":"Listener"}"#;
const OWNED: &str =
    r#"{"urn":"soundcloud:playlists:8","title":"List","user":{"urn":"soundcloud:users:7"}}"#;

struct Request {
    method: String,
    path: String,
    headers: String,
    body: String,
}

struct Reply {
    status: u16,
    body: String,
    delay: Duration,
    location: Option<String>,
}

impl Reply {
    fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            delay: Duration::ZERO,
            location: None,
        }
    }

    fn redirect(location: String) -> Self {
        Self {
            status: 302,
            body: String::new(),
            delay: Duration::ZERO,
            location: Some(location),
        }
    }
}

struct Fixture {
    client: Client,
    dir: PathBuf,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(mut route: impl FnMut(Request) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut scratch = [0u8; 4096];
                let (head_end, length) = loop {
                    let count = socket.read(&mut scratch).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&scratch[..count]);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        break (end + 4, length);
                    }
                    assert!(bytes.len() < 65_536);
                };
                while bytes.len() < head_end + length {
                    let count = socket.read(&mut scratch).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&scratch[..count]);
                }
                let headers = String::from_utf8(bytes[..head_end].to_vec()).unwrap();
                let mut parts = headers.lines().next().unwrap().split_whitespace();
                let request = Request {
                    method: parts.next().unwrap().into(),
                    path: parts.next().unwrap().into(),
                    body: String::from_utf8(bytes[head_end..head_end + length].to_vec()).unwrap(),
                    headers,
                };
                let reply = route(request);
                tokio::time::sleep(reply.delay).await;
                let location = reply
                    .location
                    .map(|location| format!("Location: {location}\r\n"))
                    .unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{location}Connection: close\r\n\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("soundcloud-test-{:016x}", rand::random::<u64>()));
        let dirs = crate::paths::AppDirs {
            config: dir.join("config"),
            state: dir.join("state"),
            cache: dir.join("cache"),
        };
        std::fs::create_dir_all(&dirs.state).unwrap();
        let store = credentials::Store::in_memory(dirs);
        let transport = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let base = Url::parse(&format!("http://{address}/")).unwrap();
        let client = Client::with_endpoints(
            Http::new(transport.clone()),
            Http::new(transport),
            store,
            base.clone(),
            base.join("oauth/token").unwrap(),
        );
        let grant = dummy_grant();
        let lease = client.inner.store.lease(Slot::SoundCloud);
        lease
            .save(credentials::Grant::SoundCloud(grant.clone()))
            .await
            .unwrap();
        *client.inner.session.lock().await = Some(Session {
            grant,
            lease,
            refresh_blocked: false,
        });
        Self {
            client,
            dir,
            server,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        // The path is a fixed workspace child followed only by a random number.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn dummy_grant() -> StoredGrant {
    StoredGrant {
        app: AppCredentials::new("dummy-app".into(), "dummy-secret".into()).unwrap(),
        access_token: "dummy-access".into(),
        refresh_token: "dummy-refresh".into(),
        expires_at: u64::MAX,
    }
}

#[tokio::test]
async fn stream_redirect_authenticates_only_the_api_origin_and_preserves_range() {
    let cdn_requests = Arc::new(AtomicUsize::new(0));
    let received = cdn_requests.clone();
    let cdn = Fixture::new(move |request| {
        received.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/audio.aac?signature=dummy-signed-url");
        let headers = request.headers.to_ascii_lowercase();
        assert!(!headers.contains("authorization:"));
        assert!(!headers.contains("dummy-access"));
        assert!(headers.contains("range: bytes=4-7"));
        let mut reply = Reply::json("data");
        reply.status = 206;
        reply
    })
    .await;
    let cdn_url = cdn
        .client
        .inner
        .base
        .join("audio.aac?signature=dummy-signed-url")
        .unwrap();
    let api_requests = Arc::new(AtomicUsize::new(0));
    let received = api_requests.clone();
    let fixture = Fixture::new(move |request| {
        received.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.path, "/tracks/soundcloud:tracks:1/streams/uuid/hls");
        let headers = request.headers.to_ascii_lowercase();
        assert!(headers.contains("authorization: oauth dummy-access"));
        assert!(headers.contains("range: bytes=4-7"));
        Reply::redirect(cdn_url.to_string())
    })
    .await;
    let url = fixture
        .client
        .inner
        .base
        .join("tracks/soundcloud:tracks:1/streams/uuid/hls")
        .unwrap();
    for invalid_range in [(0, 0), (u64::MAX, 2)] {
        assert_eq!(
            fixture
                .client
                .stream_response(url.clone(), Some(invalid_range))
                .await
                .unwrap_err(),
            Error::InvalidResource
        );
    }
    assert_eq!(api_requests.load(Ordering::SeqCst), 0);
    let response = fixture
        .client
        .stream_response(url, Some((4, 4)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"data");
    assert_eq!(api_requests.load(Ordering::SeqCst), 1);
    assert_eq!(cdn_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stream_api_unauthorized_refreshes_once_across_the_entire_redirect_chain() {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let rotated = refreshes.clone();
    let api_requests = Arc::new(AtomicUsize::new(0));
    let received = api_requests.clone();
    let fixture = Fixture::new(move |request| {
        if request.path == "/oauth/token" {
            assert_eq!(request.method, "POST");
            assert!(request.body.contains("refresh_token=dummy-refresh"));
            rotated.fetch_add(1, Ordering::SeqCst);
            return Reply::json(
                r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
            );
        }
        received.fetch_add(1, Ordering::SeqCst);
        let headers = request.headers.to_ascii_lowercase();
        if request.path == "/stream" && headers.contains("authorization: oauth new-access") {
            return Reply::redirect("/still-rejected".into());
        }
        if request.path == "/still-rejected" {
            assert!(headers.contains("authorization: oauth new-access"));
        } else {
            assert_eq!(request.path, "/stream");
            assert!(headers.contains("authorization: oauth dummy-access"));
        }
        let mut reply = Reply::json("{}");
        reply.status = 401;
        reply
    })
    .await;
    let url = fixture.client.inner.base.join("stream").unwrap();
    assert_eq!(
        fixture.client.stream_response(url, None).await.unwrap_err(),
        Error::SignInExpired
    );
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(api_requests.load(Ordering::SeqCst), 3);
    let loaded = fixture
        .client
        .inner
        .store
        .lease(Slot::SoundCloud)
        .load()
        .await
        .unwrap();
    let Some(credentials::Grant::SoundCloud(grant)) = loaded.grant else {
        panic!("missing rotated grant");
    };
    assert_eq!(grant.refresh_token, "new-refresh");
}

#[tokio::test]
async fn stream_cdn_unauthorized_never_sends_or_refreshes_the_account_token() {
    let api_requests = Arc::new(AtomicUsize::new(0));
    let received = api_requests.clone();
    let fixture = Fixture::new(move |_| {
        received.fetch_add(1, Ordering::SeqCst);
        panic!("a CDN rejection must not contact the API or token endpoint");
    })
    .await;
    let cdn_requests = Arc::new(AtomicUsize::new(0));
    let received = cdn_requests.clone();
    let cdn = Fixture::new(move |request| {
        received.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.path, "/expired.aac");
        assert!(
            !request
                .headers
                .to_ascii_lowercase()
                .contains("authorization:")
        );
        let mut reply = Reply::json("{}");
        reply.status = 401;
        reply
    })
    .await;
    let url = cdn.client.inner.base.join("expired.aac").unwrap();
    let response = fixture.client.stream_response(url, None).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(api_requests.load(Ordering::SeqCst), 0);
    assert_eq!(cdn_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .client
            .inner
            .session
            .lock()
            .await
            .as_ref()
            .unwrap()
            .grant
            .refresh_token,
        "dummy-refresh"
    );
}

#[tokio::test]
async fn stream_redirects_reject_unsafe_targets_before_another_request() {
    for target in [
        "http://example.com/audio.aac",
        "https://user:password@api.soundcloud.com/audio.aac",
        "https://api.soundcloud.com/audio.aac#fragment",
    ] {
        let requests = Arc::new(AtomicUsize::new(0));
        let received = requests.clone();
        let fixture = Fixture::new(move |request| {
            received.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.path, "/stream");
            assert!(
                request
                    .headers
                    .to_ascii_lowercase()
                    .contains("authorization: oauth dummy-access")
            );
            Reply::redirect(target.into())
        })
        .await;
        let url = fixture.client.inner.base.join("stream").unwrap();
        assert_eq!(
            fixture.client.stream_response(url, None).await.unwrap_err(),
            Error::InvalidResource
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }
    let requests = Arc::new(AtomicUsize::new(0));
    let received = requests.clone();
    let fixture = Fixture::new(move |_| {
        received.fetch_add(1, Ordering::SeqCst);
        Reply::redirect("/stream".into())
    })
    .await;
    let url = fixture.client.inner.base.join("stream").unwrap();
    assert_eq!(
        fixture.client.stream_response(url, None).await.unwrap_err(),
        Error::InvalidResponse
    );
    assert_eq!(requests.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn concurrent_requests_rotate_a_single_use_refresh_token_once() {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let captured = refreshes.clone();
    let fixture = Fixture::new(move |request| {
        if request.path == "/oauth/token" {
            assert_eq!(request.method, "POST");
            assert!(request.body.contains("refresh_token=dummy-refresh"));
            captured.fetch_add(1, Ordering::SeqCst);
            Reply::json(
                r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
            )
        } else {
            assert_eq!(request.path, "/me");
            assert!(
                request
                    .headers
                    .to_ascii_lowercase()
                    .contains("authorization: oauth new-access")
            );
            Reply::json(USER)
        }
    })
    .await;
    fixture
        .client
        .inner
        .session
        .lock()
        .await
        .as_mut()
        .unwrap()
        .grant
        .expires_at = 0;
    let (one, two) = tokio::join!(fixture.client.me(), fixture.client.me());
    assert!(one.is_ok() && two.is_ok());
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    let loaded = fixture
        .client
        .inner
        .store
        .lease(Slot::SoundCloud)
        .load()
        .await
        .unwrap();
    let Some(credentials::Grant::SoundCloud(grant)) = loaded.grant else {
        panic!("missing grant");
    };
    assert_eq!(grant.refresh_token, "new-refresh");
}

#[tokio::test]
async fn cancelling_a_search_does_not_lose_a_consumed_refresh_token() {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let captured = refreshes.clone();
    let (announced, begun) = tokio::sync::oneshot::channel();
    let mut announced = Some(announced);
    let fixture = Fixture::new(move |request| {
        if request.path == "/oauth/token" {
            captured.fetch_add(1, Ordering::SeqCst);
            if let Some(announced) = announced.take() {
                let _ = announced.send(());
            }
            let mut reply = Reply::json(
                r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
            );
            reply.delay = Duration::from_millis(50);
            reply
        } else {
            assert!(
                request
                    .headers
                    .to_ascii_lowercase()
                    .contains("authorization: oauth new-access")
            );
            Reply::json(USER)
        }
    })
    .await;
    fixture
        .client
        .inner
        .session
        .lock()
        .await
        .as_mut()
        .unwrap()
        .grant
        .expires_at = 0;
    let client = fixture.client.clone();
    let cancelled = tokio::spawn(async move { client.me().await });
    begun.await.unwrap();
    cancelled.abort();
    assert!(fixture.client.me().await.is_ok());
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    let loaded = fixture
        .client
        .inner
        .store
        .lease(Slot::SoundCloud)
        .load()
        .await
        .unwrap();
    let Some(credentials::Grant::SoundCloud(grant)) = loaded.grant else {
        panic!("missing rotated grant");
    };
    assert_eq!(grant.refresh_token, "new-refresh");
}

#[tokio::test]
async fn ambiguous_refresh_is_not_replayed_or_restored() {
    let requests = Arc::new(AtomicUsize::new(0));
    let captured = requests.clone();
    let fixture = Fixture::new(move |_| {
        captured.fetch_add(1, Ordering::SeqCst);
        Reply {
            status: 500,
            body: "{\"error\":\"dummy-secret\"}".into(),
            delay: Duration::ZERO,
            location: None,
        }
    })
    .await;
    fixture
        .client
        .inner
        .session
        .lock()
        .await
        .as_mut()
        .unwrap()
        .grant
        .expires_at = 0;
    let error = fixture.client.me().await.unwrap_err();
    assert_eq!(error, Error::Status { status: 500 });
    assert!(!error.to_string().contains("dummy-secret"));
    assert_eq!(fixture.client.me().await.unwrap_err(), Error::SignInExpired);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(
        fixture
            .client
            .inner
            .store
            .lease(Slot::SoundCloud)
            .load()
            .await
            .unwrap()
            .grant
            .is_none()
    );
}

#[tokio::test]
async fn an_invalid_proxy_does_not_consume_or_revoke_a_refresh_token() {
    let fixture = Fixture::new(|_| panic!("a blocked client must not reach the network")).await;
    fixture
        .client
        .inner
        .session
        .lock()
        .await
        .as_mut()
        .unwrap()
        .grant
        .expires_at = 0;
    fixture.client.inner.mutations.block("invalid proxy".into());
    assert_eq!(
        fixture.client.me().await.unwrap_err(),
        Error::ProxyConfiguration
    );
    assert!(
        fixture
            .client
            .inner
            .store
            .lease(Slot::SoundCloud)
            .load()
            .await
            .unwrap()
            .grant
            .is_some()
    );
    assert!(fixture.client.is_signed_in().await);
}

#[tokio::test]
async fn pagination_rejects_another_origin_before_sending_a_token() {
    let requests = Arc::new(AtomicUsize::new(0));
    let captured = requests.clone();
    let fixture = Fixture::new(move |_| {
        captured.fetch_add(1, Ordering::SeqCst);
        Reply::json(r#"{"collection":[],"next_href":"https://example.invalid/steal"}"#)
    })
    .await;
    assert_eq!(
        fixture
            .client
            .playlist_tracks("soundcloud:playlists:8")
            .await
            .unwrap_err(),
        Error::InvalidResource
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_stale_playlist_edit_never_sends_a_write() {
    let writes = Arc::new(AtomicUsize::new(0));
    let captured = writes.clone();
    let fixture = Fixture::new(move |request| {
        if request.method != "GET" {
            captured.fetch_add(1, Ordering::SeqCst);
        }
        if request.path == "/me" {
            Reply::json(USER)
        } else if request.path.contains("/tracks?") {
            Reply::json(r#"{"collection":[{"urn":"soundcloud:tracks:2"}]}"#)
        } else {
            Reply::json(OWNED)
        }
    })
    .await;
    let error = fixture
        .client
        .replace_playlist_tracks(
            "soundcloud:playlists:8",
            &["soundcloud:tracks:1".into()],
            &["soundcloud:tracks:3".into()],
        )
        .await
        .unwrap_err();
    assert_eq!(error, Error::Conflict);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn playlist_replacements_preserve_duplicate_occurrences_and_order() {
    let order = Arc::new(StdMutex::new(vec![
        "soundcloud:tracks:1".to_owned(),
        "soundcloud:tracks:1".into(),
        "soundcloud:tracks:2".into(),
    ]));
    let remote = order.clone();
    let fixture = Fixture::new(move |request| {
        if request.path == "/me" { return Reply::json(USER); }
        if request.method == "PUT" {
            let value: Value = serde_json::from_str(&request.body).unwrap();
            *remote.lock().unwrap() = value["playlist"]["tracks"].as_array().unwrap().iter().map(|track| track["urn"].as_str().unwrap().into()).collect();
            return Reply::json(OWNED);
        }
        if request.path.contains("/tracks?") {
            return Reply::json(json!({"collection": remote.lock().unwrap().iter().map(|urn| json!({"urn":urn})).collect::<Vec<_>>()}).to_string());
        }
        Reply::json(OWNED)
    }).await;
    let before = order.lock().unwrap().clone();
    let desired = vec![
        "soundcloud:tracks:2".into(),
        "soundcloud:tracks:1".into(),
        "soundcloud:tracks:1".into(),
    ];
    let result = fixture
        .client
        .replace_playlist_tracks("soundcloud:playlists:8", &before, &desired)
        .await
        .unwrap();
    assert!(same_order(&result.tracks, &desired));
    assert_eq!(*order.lock().unwrap(), desired);
}

#[tokio::test]
async fn concurrent_appends_fetch_and_write_inside_one_playlist_lock() {
    let order = Arc::new(StdMutex::new(vec!["soundcloud:tracks:1".to_owned()]));
    let remote = order.clone();
    let fixture = Fixture::new(move |request| {
        if request.path == "/me" { return Reply::json(USER); }
        if request.method == "PUT" {
            let value: Value = serde_json::from_str(&request.body).unwrap();
            *remote.lock().unwrap() = value["playlist"]["tracks"].as_array().unwrap().iter().map(|track| track["urn"].as_str().unwrap().into()).collect();
            return Reply::json(OWNED);
        }
        if request.path.contains("/tracks?") {
            return Reply::json(json!({"collection": remote.lock().unwrap().iter().map(|urn| json!({"urn":urn})).collect::<Vec<_>>()}).to_string());
        }
        Reply::json(OWNED)
    }).await;
    let two = ["soundcloud:tracks:2".into()];
    let three = ["soundcloud:tracks:3".into()];
    let (first, second) = tokio::join!(
        fixture
            .client
            .append_playlist_tracks("soundcloud:playlists:8", &two),
        fixture
            .client
            .append_playlist_tracks("soundcloud:playlists:8", &three)
    );
    assert!(first.is_ok() && second.is_ok());
    let order = order.lock().unwrap();
    assert_eq!(order.len(), 3);
    assert_eq!(order[0], "soundcloud:tracks:1");
    assert!(order[1..].contains(&"soundcloud:tracks:2".to_owned()));
    assert!(order[1..].contains(&"soundcloud:tracks:3".to_owned()));
}

#[tokio::test]
async fn disconnect_revokes_only_soundcloud_and_stops_restoration() {
    let fixture = Fixture::new(|request| {
        assert_eq!(request.path, "/disconnect");
        Reply {
            status: 204,
            body: String::new(),
            delay: Duration::ZERO,
            location: None,
        }
    })
    .await;
    let spotify = credentials::Grant::Web(crate::auth::StoredToken {
        client_id: "dummy".into(),
        access_token: "spotify-access".into(),
        refresh_token: "spotify-refresh".into(),
        ..Default::default()
    });
    fixture
        .client
        .inner
        .store
        .lease(Slot::Personal)
        .save(spotify.clone())
        .await
        .unwrap();
    fixture.client.disconnect().await.unwrap();
    assert!(!fixture.client.is_signed_in().await);
    assert!(
        fixture
            .client
            .inner
            .store
            .lease(Slot::SoundCloud)
            .load()
            .await
            .unwrap()
            .grant
            .is_none()
    );
    assert!(
        fixture
            .client
            .inner
            .store
            .lease(Slot::Personal)
            .load()
            .await
            .unwrap()
            .grant
            == Some(spotify)
    );
}

#[tokio::test]
async fn expired_disconnect_is_locally_complete_without_claiming_remote_revocation() {
    let requests = Arc::new(AtomicUsize::new(0));
    let captured = requests.clone();
    let fixture = Fixture::new(move |request| {
        captured.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/disconnect");
        Reply {
            status: 401,
            body: r#"{"error":"dummy-secret"}"#.into(),
            delay: Duration::ZERO,
            location: None,
        }
    })
    .await;
    fixture
        .client
        .inner
        .session
        .lock()
        .await
        .as_mut()
        .unwrap()
        .grant
        .expires_at = 0;
    let error = fixture.client.disconnect().await.unwrap_err();
    assert_eq!(error, Error::RemoteRevocationUnconfirmed);
    assert!(!error.to_string().contains("dummy-secret"));
    assert!(!fixture.client.is_signed_in().await);
    assert!(
        fixture
            .client
            .inner
            .store
            .lease(Slot::SoundCloud)
            .load()
            .await
            .unwrap()
            .grant
            .is_none()
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_blocked_proxy_cannot_leave_disconnected_credentials_restorable() {
    let fixture = Fixture::new(|_| panic!("a blocked proxy must not contact the network")).await;
    fixture.client.inner.mutations.block("invalid proxy".into());
    assert_eq!(
        fixture.client.disconnect().await.unwrap_err(),
        Error::ProxyConfiguration
    );
    assert!(!fixture.client.is_signed_in().await);
    assert!(
        fixture
            .client
            .inner
            .store
            .lease(Slot::SoundCloud)
            .load()
            .await
            .unwrap()
            .grant
            .is_none()
    );
}

#[tokio::test]
async fn cancelling_after_oauth_commit_prevents_restore_and_preserves_a_new_sign_in() {
    let fixture =
        Fixture::new(|_| panic!("cancelling must not revoke a later remote sign-in")).await;
    assert!(fixture.client.is_signed_in().await);
    let mut held_session = fixture.client.inner.session.lock().await;
    fixture.client.cancel_authorization().unwrap();
    assert!(!held_session.as_ref().unwrap().lease.current());
    assert!(
        fixture
            .client
            .inner
            .store
            .lease(Slot::SoundCloud)
            .load()
            .await
            .unwrap()
            .grant
            .is_none()
    );

    // A replacement authorization starts before old memory cleanup finishes.
    fixture.client.cancel_sign_in();
    fixture.client.inner.store.invalidate(Slot::SoundCloud);
    let lease = fixture.client.inner.store.lease(Slot::SoundCloud);
    let mut grant = dummy_grant();
    grant.access_token = "replacement-access".into();
    lease
        .save(credentials::Grant::SoundCloud(grant.clone()))
        .await
        .unwrap();
    *held_session = Some(Session {
        grant,
        lease,
        refresh_blocked: false,
    });
    drop(held_session);
    tokio::task::yield_now().await;
    assert!(fixture.client.is_signed_in().await);
    let loaded = fixture
        .client
        .inner
        .store
        .lease(Slot::SoundCloud)
        .load()
        .await
        .unwrap();
    let Some(credentials::Grant::SoundCloud(grant)) = loaded.grant else {
        panic!("replacement grant missing");
    };
    assert_eq!(grant.access_token, "replacement-access");
}

#[tokio::test]
async fn non_owned_playlists_cannot_be_renamed() {
    let fixture = Fixture::new(|request| {
        assert_eq!(request.method, "GET");
        if request.path == "/me" {
            Reply::json(USER)
        } else {
            Reply::json(r#"{"urn":"soundcloud:playlists:8","user":{"urn":"soundcloud:users:99"}}"#)
        }
    })
    .await;
    assert_eq!(
        fixture
            .client
            .rename_playlist("soundcloud:playlists:8", "Changed")
            .await
            .unwrap_err(),
        Error::NotOwned
    );
}

#[tokio::test]
async fn queued_playlist_edit_cannot_cross_an_authorization_generation() {
    let requests = Arc::new(AtomicUsize::new(0));
    let captured = requests.clone();
    let fixture = Fixture::new(move |_| {
        captured.fetch_add(1, Ordering::SeqCst);
        Reply::json(OWNED)
    })
    .await;
    let lock = fixture.client.playlist_lock("soundcloud:playlists:8").await;
    let held = lock.lock().await;
    let client = fixture.client.clone();
    let (started, running) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(async move {
        let _ = started.send(());
        client
            .rename_playlist("soundcloud:playlists:8", "Changed")
            .await
    });
    running.await.unwrap();
    fixture.client.cancel_sign_in();
    drop(held);
    assert_eq!(pending.await.unwrap().unwrap_err(), Error::Cancelled);
    assert_eq!(requests.load(Ordering::SeqCst), 0);
}

#[test]
fn identifiers_cannot_inject_paths_or_queries() {
    for urn in [
        "soundcloud:tracks:1?secret=bad",
        "soundcloud:tracks:1/streams",
        "https://example.test",
        "1",
        "soundcloud:playlists:8",
    ] {
        assert!(Client::resource_path("tracks", urn).is_err());
    }
    assert_eq!(
        Client::resource_path("tracks", "soundcloud:tracks:123").unwrap(),
        "tracks/soundcloud:tracks:123"
    );
}
