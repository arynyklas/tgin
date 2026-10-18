//! The gateway's file modes for self-hosted Bot API servers end to end:
//! files served from the server's directory mounted in tgin, proxied to an
//! operator's file origin, or not served at all, and local mode's absolute
//! `getFile` paths rewritten to the bot's own relative ones. The app runs in
//! process on loopback sockets the OS picks; wiremock plays the server and
//! the file origin. The cloud file route is `gateway.rs`'s
//! `file_route_streams_with_range`.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tgin::app::{App, Boot, secrets};
use tgin::exit::Outcome;
use tgin_runtime::config::MapEnv;
use tgin_runtime::lifecycle::{SignalKind, Signals};
use tgin_runtime::obs::MetricsRegistry;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Bounds every wait, so a stall fails the test instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(10);
const TOKEN: &str = "123456789:AAAAexampleSecretTokenValue_-xyz";
const OTHER_TOKEN: &str = "987654321:BBBBanotherSecretTokenValue_-xyz";
const THIRD_TOKEN: &str = "555555555:CCCCthirdSecretTokenValue_-xyz-abc";
const FOURTH_TOKEN: &str = "444444444:DDDDfourthSecretTokenValue_-xyz-abc";
const CONTENT: &[u8] = b"0123456789";

/// Everything every test logs, unredacted: the test subscriber has no
/// redactor, so a token or a token directory that reaches a log line shows
/// up here.
static LOGS: LazyLock<Logs> = LazyLock::new(|| {
    let logs = Logs::default();
    tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        // wiremock logs an unmatched request whole, URL and all; those
        // lines are not tgin's.
        .with_env_filter(tracing_subscriber::EnvFilter::new("trace,wiremock=off"))
        .try_init()
        .expect("the only global subscriber");
    logs
});

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("logs")).into_owned()
    }
}

impl io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("logs").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A fresh scratch directory under the target directory.
fn scratch(prefix: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("{prefix}-{:016x}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// `token` with ':' replaced by '~'.
fn tilde(token: &str) -> String {
    token.replace(':', "~")
}

/// Creates the bot directory of `token` under `root` as telegram-bot-api
/// does: the token as it is, or its '~' form where the filesystem refuses
/// ':' (Windows). Returns the directory.
fn bot_dir(root: &Path, token: &str) -> PathBuf {
    let colon = root.join(token);
    match std::fs::create_dir(&colon) {
        Ok(()) => colon,
        Err(_) => {
            let tilde = root.join(tilde(token));
            std::fs::create_dir(&tilde).expect("the '~' bot directory");
            tilde
        }
    }
}

/// Writes `content` to `dir/rel`, creating its parents.
fn put(dir: &Path, rel: &str, content: &[u8]) {
    let file = dir.join(rel);
    std::fs::create_dir_all(file.parent().expect("a parent")).expect("parents");
    std::fs::write(&file, content).expect("the file");
}

/// The app, running on signals the test sends.
struct Tgin {
    addr: SocketAddr,
    signals: mpsc::Sender<SignalKind>,
    run: JoinHandle<Outcome>,
    metrics: Arc<MetricsRegistry>,
    http: reqwest::Client,
}

async fn start(yaml: &str) -> Tgin {
    LazyLock::force(&LOGS);
    let file = scratch("gateway-files").join("tgin.yaml");
    std::fs::write(&file, yaml).expect("the config file");
    let boot = Boot::load(&file, Arc::new(MapEnv::default()))
        .unwrap_or_else(|err| panic!("invalid test config: {err:?}"));
    let app = App::start(boot).await.expect("tgin starts");
    let addr = app.data_addrs()[0];
    let metrics = app.metrics();
    let (signals, rx) = mpsc::channel(4);
    let run = tokio::spawn(app.run(Signals::from_channel(rx)));
    Tgin {
        addr,
        signals,
        run,
        metrics,
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
    }
}

impl Tgin {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// GETs `path` with `headers`.
    async fn get(&self, path: &str, headers: &[(&str, &str)]) -> reqwest::Response {
        let mut request = self.http.get(self.url(path));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        timeout(PATIENCE, request.send())
            .await
            .expect("an answer in time")
            .expect("a response")
    }

    /// `/file/bot<token>/<rel>`, the token's ':' percent-encoded as
    /// clients send it.
    async fn file(&self, token: &str, rel: &str, headers: &[(&str, &str)]) -> reqwest::Response {
        let encoded = token.replace(':', "%3A");
        self.get(&format!("/file/bot{encoded}/{rel}"), headers)
            .await
    }

    /// Sends `body` of type `content_type` (none when empty) to `path`
    /// with `method`.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        content_type: &str,
        body: impl Into<reqwest::Body>,
    ) -> reqwest::Response {
        let mut request = self.http.request(method, self.url(path)).body(body);
        if !content_type.is_empty() {
            request = request.header("content-type", content_type);
        }
        timeout(PATIENCE, request.send())
            .await
            .expect("an answer in time")
            .expect("a response")
    }

    #[track_caller]
    fn assert_metric(&self, line: &str) {
        let out = self.metrics.encode();
        assert!(out.lines().any(|l| l == line), "missing {line:?} in\n{out}");
    }

    async fn stop(self) {
        self.signals
            .send(SignalKind::Terminate)
            .await
            .expect("the app listens");
        let outcome = timeout(PATIENCE, self.run)
            .await
            .expect("the app exits in time")
            .expect("the app does not panic");
        assert_eq!(outcome, Outcome::Clean);
    }
}

async fn description(response: reqwest::Response) -> (u16, String) {
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("body");
    let value: Value = serde_json::from_slice(&body).unwrap_or_else(|_| panic!("JSON: {body:?}"));
    (
        status,
        value["description"].as_str().unwrap_or_default().to_owned(),
    )
}

/// Asserts that no log line holds a token, in either directory form.
#[track_caller]
fn assert_logs_clean(tokens: &[&str]) {
    let logs = LOGS.text();
    for token in tokens {
        assert!(!logs.contains(token), "a token reached the logs");
        assert!(
            !logs.contains(&tilde(token)),
            "a '~' token directory reached the logs"
        );
        let secret = token.split_once(':').expect("a token").1;
        assert!(!logs.contains(secret), "a token's secret reached the logs");
    }
}

/// Bots `gw` (`TOKEN`) and `other` (`OTHER_TOKEN`) on the local upstream
/// `tba` at `api`, its files mounted at `dir`.
fn disk_config(api: &str, dir: &Path) -> String {
    format!(
        r#"version: 1
runtime: {{ lame_duck: 0s, shutdown_grace: 2s }}
server: {{ listen: ["127.0.0.1:0"] }}
admin: {{ listen: ["127.0.0.1:0"] }}
upstreams:
  tba: {{ kind: local, url: "{api}", local_mode: true, files: {{ dir: '{dir}' }} }}
bots:
  - name: gw
    token: "{TOKEN}"
    upstream: tba
    routes: [{{ to: [workers] }}]
  - name: other
    token: "{OTHER_TOKEN}"
    upstream: tba
    routes: [{{ to: [others] }}]
consumers:
  workers: {{ pull: {{}} }}
  others: {{ pull: {{}} }}
"#,
        dir = dir.display()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disk_file_served_with_range_and_conditionals() {
    let server = MockServer::start().await;
    let root = scratch("files");
    // `gw` in whichever form the filesystem takes, `other` in the '~' form
    // on every platform: tgin finds either.
    put(&bot_dir(&root, TOKEN), "documents/file_1.pdf", CONTENT);
    let other = root.join(tilde(OTHER_TOKEN));
    put(&other, "photos/file_2.jpg", b"other");
    let tgin = start(&disk_config(&server.uri(), &root)).await;

    let full = tgin.file(TOKEN, "documents/file_1.pdf", &[]).await;
    assert_eq!(full.status(), 200);
    let headers = full.headers().clone();
    assert_eq!(headers["content-type"], "application/octet-stream");
    assert_eq!(headers["content-length"], "10");
    assert_eq!(headers["accept-ranges"], "bytes");
    let etag = headers["etag"].to_str().expect("ASCII").to_owned();
    assert!(etag.starts_with("W/\""), "{etag}");
    let last_modified = headers["last-modified"].to_str().expect("ASCII").to_owned();
    assert!(last_modified.ends_with(" GMT"), "{last_modified}");
    assert_eq!(&full.bytes().await.expect("body")[..], CONTENT);

    let other_file = tgin.file(OTHER_TOKEN, "photos/file_2.jpg", &[]).await;
    assert_eq!(other_file.status(), 200);
    assert_eq!(&other_file.bytes().await.expect("body")[..], b"other");

    for (range, content_range, body) in [
        ("bytes=2-5", "bytes 2-5/10", &CONTENT[2..6]),
        ("bytes=7-", "bytes 7-9/10", &CONTENT[7..]),
        ("bytes=-3", "bytes 7-9/10", &CONTENT[7..]),
        ("bytes=8-100", "bytes 8-9/10", &CONTENT[8..]),
    ] {
        let partial = tgin
            .file(TOKEN, "documents/file_1.pdf", &[("range", range)])
            .await;
        assert_eq!(partial.status(), 206, "{range}");
        let headers = partial.headers().clone();
        assert_eq!(headers["content-range"], content_range, "{range}");
        assert_eq!(headers["content-length"], body.len().to_string().as_str());
        assert_eq!(headers["content-type"], "application/octet-stream");
        assert_eq!(&partial.bytes().await.expect("body")[..], body, "{range}");
    }

    for range in ["bytes=10-", "bytes=20-30", "bytes=-0"] {
        let unsatisfiable = tgin
            .file(TOKEN, "documents/file_1.pdf", &[("range", range)])
            .await;
        assert_eq!(unsatisfiable.status(), 416, "{range}");
        assert_eq!(unsatisfiable.headers()["content-range"], "bytes */10");
    }

    // Several ranges, or one that does not parse, get the whole file.
    for range in ["bytes=0-1,4-5", "lines=1-2", "bytes=5-2"] {
        let whole = tgin
            .file(TOKEN, "documents/file_1.pdf", &[("range", range)])
            .await;
        assert_eq!(whole.status(), 200, "{range}");
        assert_eq!(&whole.bytes().await.expect("body")[..], CONTENT);
    }

    // A matching `If-Range` keeps the range; a stale one sends everything.
    let kept = tgin
        .file(
            TOKEN,
            "documents/file_1.pdf",
            &[("range", "bytes=0-1"), ("if-range", &last_modified)],
        )
        .await;
    assert_eq!(kept.status(), 206);
    let stale = tgin
        .file(
            TOKEN,
            "documents/file_1.pdf",
            &[("range", "bytes=0-1"), ("if-range", "\"stale\"")],
        )
        .await;
    assert_eq!(stale.status(), 200);

    for conditional in [
        ("if-none-match", etag.as_str()),
        ("if-none-match", "\"a\", W/\"b\", *"),
        ("if-modified-since", last_modified.as_str()),
        ("if-modified-since", "Fri, 01 Jan 2100 00:00:00 GMT"),
    ] {
        let not_modified = tgin
            .file(TOKEN, "documents/file_1.pdf", &[conditional])
            .await;
        assert_eq!(not_modified.status(), 304, "{conditional:?}");
        assert_eq!(not_modified.headers()["etag"], etag.as_str());
        assert!(not_modified.bytes().await.expect("body").is_empty());
    }
    for conditional in [
        ("if-none-match", "\"something-else\""),
        ("if-modified-since", "Thu, 01 Jan 1970 00:00:00 GMT"),
    ] {
        let modified = tgin
            .file(TOKEN, "documents/file_1.pdf", &[conditional])
            .await;
        assert_eq!(modified.status(), 200, "{conditional:?}");
    }
    // `If-None-Match` wins over `If-Modified-Since`.
    let modified = tgin
        .file(
            TOKEN,
            "documents/file_1.pdf",
            &[
                ("if-none-match", "\"something-else\""),
                ("if-modified-since", &last_modified),
            ],
        )
        .await;
    assert_eq!(modified.status(), 200);

    let missing = tgin.file(TOKEN, "documents/nope.pdf", &[]).await;
    assert_eq!(
        description(missing).await,
        (404, "Not Found: file not found".to_owned())
    );
    let directory = tgin.file(TOKEN, "documents", &[]).await;
    assert_eq!(
        description(directory).await,
        (404, "Not Found: file not found".to_owned())
    );

    // Nothing went to the Bot API server, and the route is method `file`.
    assert!(
        server
            .received_requests()
            .await
            .expect("recording")
            .is_empty()
    );
    tgin.assert_metric(r#"tgin_gateway_requests_total{bot="gw",method="file",outcome="ok"} 17"#);
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="file",outcome="rejected"} 5"#,
    );
    assert_logs_clean(&[TOKEN, OTHER_TOKEN]);
    tgin.stop().await;
}

/// On a filesystem that takes ':', both forms may exist; the colon form is
/// telegram-bot-api's own, so it comes first.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn colon_directory_preferred_over_tilde() {
    let server = MockServer::start().await;
    let root = scratch("files");
    put(&root.join(TOKEN), "documents/file_1.pdf", b"colon");
    put(&root.join(tilde(TOKEN)), "documents/file_1.pdf", b"tilde");
    let tgin = start(&disk_config(&server.uri(), &root)).await;

    let response = tgin.file(TOKEN, "documents/file_1.pdf", &[]).await;
    assert_eq!(response.status(), 200);
    assert_eq!(&response.bytes().await.expect("body")[..], b"colon");
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_confined_to_the_calling_bots_directory() {
    let server = MockServer::start().await;
    let root = scratch("files");
    let mine = bot_dir(&root, TOKEN);
    put(&mine, "documents/mine.txt", b"mine");
    // The other bot in both forms, so neither is reachable.
    let theirs = bot_dir(&root, OTHER_TOKEN);
    put(&theirs, "documents/secret.txt", b"secret");
    let theirs_tilde = root.join(tilde(OTHER_TOKEN));
    put(&theirs_tilde, "documents/secret.txt", b"secret");
    put(&root, "outside.txt", b"outside");
    let tgin = start(&disk_config(&server.uri(), &root)).await;

    let own = tgin.file(TOKEN, "documents/mine.txt", &[]).await;
    assert_eq!(own.status(), 200);
    assert_eq!(&own.bytes().await.expect("body")[..], b"mine");
    let through_theirs = tgin.file(OTHER_TOKEN, "documents/secret.txt", &[]).await;
    assert_eq!(through_theirs.status(), 200);

    // Another bot's file through this bot's token is simply not there.
    let missing = tgin.file(TOKEN, "documents/secret.txt", &[]).await;
    assert_eq!(
        description(missing).await,
        (404, "Not Found: file not found".to_owned())
    );

    let theirs_name = theirs
        .file_name()
        .expect("a name")
        .to_string_lossy()
        .into_owned();
    let theirs_encoded = theirs_name.replace(':', "%3A");
    let tilde_name = tilde(OTHER_TOKEN);
    let mut escapes = vec![
        format!("%2e%2e/{theirs_encoded}/documents/secret.txt"),
        format!("%2E%2E/{tilde_name}/documents/secret.txt"),
        format!("documents/%2e%2e/%2e%2e/{tilde_name}/documents/secret.txt"),
        format!("..%2F{tilde_name}%2Fdocuments%2Fsecret.txt"),
        format!("documents%2F..%2F..%2F{tilde_name}%2Fdocuments%2Fsecret.txt"),
        format!("%2e%2e%2f{theirs_encoded}%2fdocuments%2fsecret.txt"),
        "%2e%2e/outside.txt".to_owned(),
        "..%2Foutside.txt".to_owned(),
        "%2Foutside.txt".to_owned(),
        "documents%5C..%5C..%5Coutside.txt".to_owned(),
        "documents/mine.txt%00".to_owned(),
    ];
    let absolute = root.join("outside.txt");
    let absolute = absolute.to_string_lossy();
    escapes.push(absolute.replace('\\', "%5C").replace(':', "%3A"));
    for escape in &escapes {
        let encoded = TOKEN.replace(':', "%3A");
        let response = tgin.get(&format!("/file/bot{encoded}/{escape}"), &[]).await;
        let (status, text) = description(response).await;
        assert_eq!(status, 404, "{escape}");
        assert!(text.starts_with("Not Found"), "{escape}: {text}");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        // A directory and a file link out of the bot's directory, and one
        // that stays inside it.
        symlink(&theirs_tilde, mine.join("theirs")).expect("a directory link");
        symlink(root.join("outside.txt"), mine.join("outside.txt")).expect("a file link");
        symlink(mine.join("documents/mine.txt"), mine.join("inside.txt")).expect("a link inside");
        for rel in ["theirs/documents/secret.txt", "outside.txt"] {
            let response = tgin.file(TOKEN, rel, &[]).await;
            assert_eq!(
                description(response).await,
                (404, "Not Found".to_owned()),
                "{rel}"
            );
        }
        let inside = tgin.file(TOKEN, "inside.txt", &[]).await;
        assert_eq!(inside.status(), 200);
        assert_eq!(&inside.bytes().await.expect("body")[..], b"mine");
    }

    assert!(
        server
            .received_requests()
            .await
            .expect("recording")
            .is_empty()
    );
    assert_logs_clean(&[TOKEN, OTHER_TOKEN]);
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn origin_files_proxied() {
    let server = MockServer::start().await;
    let origin = MockServer::start().await;
    let encoded = TOKEN.replace(':', "%3A");
    Mock::given(method("GET"))
        .and(path(format!("/files/{encoded}/documents/file_1.pdf")))
        .respond_with(
            ResponseTemplate::new(206)
                .set_body_raw("2345", "application/pdf")
                .insert_header("content-range", "bytes 2-5/10")
                .insert_header("accept-ranges", "bytes")
                .insert_header("etag", "\"o1\"")
                .insert_header("set-cookie", "origin=1"),
        )
        .mount(&origin)
        .await;
    // Paths are forwarded as their decoded segments, encoded again: a
    // double-encoded dot run stays one literal segment, an encoded
    // unreserved character arrives plain.
    for (rel, body) in [
        ("%252e%252e%252fx", "literal"),
        ("documents/file_1.pdf", "plain"),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/files/{encoded}/{rel}")))
            .and(query_param("probe", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/plain"))
            .with_priority(1)
            .mount(&origin)
            .await;
    }
    let yaml = format!(
        r#"version: 1
runtime: {{ lame_duck: 0s, shutdown_grace: 2s }}
server: {{ listen: ["127.0.0.1:0"] }}
admin: {{ listen: ["127.0.0.1:0"] }}
upstreams:
  tba: {{ kind: local, url: "{api}", local_mode: true, files: {{ url: "{files}/files" }} }}
bots:
  - name: gw
    token: "{TOKEN}"
    upstream: tba
    routes: [{{ to: [workers] }}]
consumers:
  workers: {{ pull: {{}} }}
"#,
        api = server.uri(),
        files = origin.uri(),
    );
    let tgin = start(&yaml).await;

    let response = tgin
        .file(
            TOKEN,
            "documents/file_1.pdf",
            &[("range", "bytes=2-5"), ("cookie", "c=1")],
        )
        .await;
    assert_eq!(response.status(), 206);
    let headers = response.headers().clone();
    assert_eq!(headers["content-range"], "bytes 2-5/10");
    assert_eq!(headers["content-type"], "application/pdf");
    assert_eq!(headers["etag"], "\"o1\"");
    assert_eq!(headers["content-length"], "4");
    assert!(headers.get("set-cookie").is_none());
    assert_eq!(&response.bytes().await.expect("body")[..], b"2345");

    let requests = origin.received_requests().await.expect("recording");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].headers["range"], "bytes=2-5");
    assert!(requests[0].headers.get("cookie").is_none());
    assert!(requests[0].headers.get("via").is_some());
    assert!(
        server
            .received_requests()
            .await
            .expect("recording")
            .is_empty()
    );
    tgin.assert_metric(r#"tgin_gateway_requests_total{bot="gw",method="file",outcome="ok"} 1"#);

    // Traversal, encoded or not, never reaches the origin.
    for escape in ["..%2F/", "..%2F..%2Fx", "%2e%2e%2fx", "a%5C..%5Cx", "a%00b"] {
        let response = tgin.file(TOKEN, escape, &[]).await;
        assert_eq!(
            description(response).await,
            (404, "Not Found".to_owned()),
            "{escape}"
        );
    }
    assert_eq!(
        origin.received_requests().await.expect("recording").len(),
        1
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="file",outcome="rejected"} 5"#,
    );

    for (rel, expected_path, body) in [
        (
            "%252e%252e%252fx",
            format!("/files/{encoded}/%252e%252e%252fx"),
            "literal",
        ),
        (
            "documents/file%5F1.pdf",
            format!("/files/{encoded}/documents/file_1.pdf"),
            "plain",
        ),
    ] {
        let response = tgin.file(TOKEN, &format!("{rel}?probe=1"), &[]).await;
        assert_eq!(response.status(), 200, "{rel}");
        assert_eq!(&response.bytes().await.expect("body")[..], body.as_bytes());
        let requests = origin.received_requests().await.expect("recording");
        let last = requests.last().expect("a request");
        assert_eq!(last.url.path(), expected_path, "{rel}");
    }
    assert_logs_clean(&[TOKEN]);
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_files_config_answers_not_served() {
    let server = MockServer::start().await;
    let yaml = format!(
        r#"version: 1
runtime: {{ lame_duck: 0s, shutdown_grace: 2s }}
server: {{ listen: ["127.0.0.1:0"] }}
admin: {{ listen: ["127.0.0.1:0"] }}
upstreams:
  tba: {{ kind: local, url: "{api}", local_mode: true }}
bots:
  - name: gw
    token: "{TOKEN}"
    upstream: tba
    routes: [{{ to: [workers] }}]
consumers:
  workers: {{ pull: {{}} }}
"#,
        api = server.uri(),
    );
    let tgin = start(&yaml).await;

    let response = tgin.file(TOKEN, "documents/file_1.pdf", &[]).await;
    assert_eq!(
        description(response).await,
        (
            404,
            "Not Found: files are not served for this upstream".to_owned()
        )
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recording")
            .is_empty()
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="file",outcome="rejected"} 1"#,
    );
    tgin.stop().await;
}

/// A `getFile` answer with `file_path`.
fn get_file_answer(file_path: &str) -> Value {
    json!({
        "ok": true,
        "result": {
            "file_id": "AgAD",
            "file_unique_id": "AQAD",
            "file_size": 10,
            "file_path": file_path,
        }
    })
}

/// Mounts, on `server`, `token`'s `getFile` for `file_id` answering
/// `body`.
async fn mount_get_file(server: &MockServer, token: &str, file_id: &str, body: &Value) {
    Mock::given(path(format!("/bot{token}/getFile")))
        .and(query_param("file_id", file_id))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_file_rewritten_only_in_local_mode_and_only_own_prefix() {
    let server = MockServer::start().await;
    let dir = scratch("files");
    // `local` (TOKEN) is in local mode with the server's directory at
    // /srv/tba; `plain` (OTHER_TOKEN) has the same directory but no local
    // mode; `origin` (THIRD_TOKEN) is in local mode with a file origin;
    // `cloud` (FOURTH_TOKEN) is on the cloud.
    let tilde_token = tilde(TOKEN);
    let cases: Vec<(&str, &str, String, Value)> = vec![
        // (token, file_id, file_path answered, file_path expected)
        (
            TOKEN,
            "colon",
            format!("/srv/tba/{TOKEN}/documents/file_1.pdf"),
            json!("documents/file_1.pdf"),
        ),
        (
            TOKEN,
            "tilde",
            format!("/srv/tba/{tilde_token}/photos/file_2.jpg"),
            json!("photos/file_2.jpg"),
        ),
        (
            THIRD_TOKEN,
            "origin-windows",
            format!("C:\\tba\\{THIRD_TOKEN}\\videos\\file_0.mp4"),
            json!("videos/file_0.mp4"),
        ),
        (
            TOKEN,
            "escaped",
            format!("/srv/tba/{TOKEN}/music/\"q\" é.mp3"),
            json!("music/\"q\" é.mp3"),
        ),
        // Another bot's directory, a relative path, another root, and the
        // bot's directory alone: verbatim.
        (
            TOKEN,
            "theirs",
            format!("/srv/tba/{OTHER_TOKEN}/documents/file_3.pdf"),
            json!(format!("/srv/tba/{OTHER_TOKEN}/documents/file_3.pdf")),
        ),
        (
            TOKEN,
            "relative",
            "documents/file_4.pdf".to_owned(),
            json!("documents/file_4.pdf"),
        ),
        (
            TOKEN,
            "elsewhere",
            format!("/var/tba/{TOKEN}/documents/file_5.pdf"),
            json!(format!("/var/tba/{TOKEN}/documents/file_5.pdf")),
        ),
        (
            TOKEN,
            "dir-only",
            format!("/srv/tba/{TOKEN}/"),
            json!(format!("/srv/tba/{TOKEN}/")),
        ),
        (
            TOKEN,
            "prefix-of-root",
            format!("/srv/tba2/{TOKEN}/documents/f"),
            json!(format!("/srv/tba2/{TOKEN}/documents/f")),
        ),
        // Not in local mode: verbatim.
        (
            OTHER_TOKEN,
            "plain",
            format!("/srv/tba/{OTHER_TOKEN}/documents/file_6.pdf"),
            json!(format!("/srv/tba/{OTHER_TOKEN}/documents/file_6.pdf")),
        ),
        // A file origin: the bot's own directory, wherever the server's is.
        (
            THIRD_TOKEN,
            "origin",
            format!("/var/lib/telegram-bot-api/{THIRD_TOKEN}/videos/file_7.mp4"),
            json!("videos/file_7.mp4"),
        ),
        (
            THIRD_TOKEN,
            "origin-theirs",
            format!("/var/lib/telegram-bot-api/{TOKEN}/videos/file_8.mp4"),
            json!(format!(
                "/var/lib/telegram-bot-api/{TOKEN}/videos/file_8.mp4"
            )),
        ),
        (
            THIRD_TOKEN,
            "origin-relative",
            "videos/file_9.mp4".to_owned(),
            json!("videos/file_9.mp4"),
        ),
        // The cloud: verbatim.
        (
            FOURTH_TOKEN,
            "cloud",
            format!("/srv/tba/{FOURTH_TOKEN}/documents/file_10.pdf"),
            json!(format!("/srv/tba/{FOURTH_TOKEN}/documents/file_10.pdf")),
        ),
    ];
    for (token, file_id, file_path, _) in &cases {
        mount_get_file(&server, token, file_id, &get_file_answer(file_path)).await;
    }
    // A failed getFile, another method with a file path, and an answer
    // over 64 KiB pass verbatim.
    let failed = json!({ "ok": false, "error_code": 400, "description": format!("Bad Request: /srv/tba/{TOKEN}/x") });
    Mock::given(path(format!("/bot{TOKEN}/getFile")))
        .and(query_param("file_id", "failed"))
        .respond_with(ResponseTemplate::new(400).set_body_json(&failed))
        .mount(&server)
        .await;
    let other_method = get_file_answer(&format!("/srv/tba/{TOKEN}/documents/file_1.pdf"));
    Mock::given(path(format!("/bot{TOKEN}/getStickerSet")))
        .respond_with(ResponseTemplate::new(200).set_body_json(&other_method))
        .mount(&server)
        .await;
    let mut large = get_file_answer(&format!("/srv/tba/{TOKEN}/documents/large.pdf"));
    large["result"]["padding"] = json!("x".repeat(70 * 1024));
    mount_get_file(&server, TOKEN, "large", &large).await;

    let yaml = format!(
        r#"version: 1
runtime: {{ lame_duck: 0s, shutdown_grace: 2s }}
server: {{ listen: ["127.0.0.1:0"] }}
admin: {{ listen: ["127.0.0.1:0"] }}
upstreams:
  local: {{ kind: local, url: "{api}", local_mode: true, files: {{ dir: '{dir}', server_dir: /srv/tba }} }}
  plain: {{ kind: local, url: "{api}", files: {{ dir: '{dir}', server_dir: /srv/tba }} }}
  origin: {{ kind: local, url: "{api}", local_mode: true, files: {{ url: "{api}/files" }} }}
bots:
  - name: local
    token: "{TOKEN}"
    upstream: local
    routes: [{{ to: [a] }}]
  - name: plain
    token: "{OTHER_TOKEN}"
    upstream: plain
    routes: [{{ to: [b] }}]
  - name: origin
    token: "{THIRD_TOKEN}"
    upstream: origin
    routes: [{{ to: [c] }}]
  - name: cloud
    token: "{FOURTH_TOKEN}"
    api_url: "{api}"
    routes: [{{ to: [d] }}]
consumers:
  a: {{ pull: {{}} }}
  b: {{ pull: {{}} }}
  c: {{ pull: {{}} }}
  d: {{ pull: {{}} }}
"#,
        api = server.uri(),
        dir = dir.display(),
    );
    let tgin = start(&yaml).await;

    for (token, file_id, _, expected) in &cases {
        let response = tgin
            .get(
                &format!("/bot{token}/getFile?file_id={file_id}"),
                &[("accept-encoding", "gzip")],
            )
            .await;
        assert_eq!(response.status(), 200, "{file_id}");
        let length: usize = response.headers()["content-length"]
            .to_str()
            .expect("ASCII")
            .parse()
            .expect("a length");
        let body = response.bytes().await.expect("body");
        assert_eq!(body.len(), length, "{file_id}");
        let value: Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(value["result"]["file_path"], *expected, "{file_id}");
        assert_eq!(value["result"]["file_id"], "AgAD", "{file_id}");
        assert_eq!(value["result"]["file_size"], 10, "{file_id}");
        assert_eq!(value["ok"], true, "{file_id}");
    }

    // A rewrite changes `file_path` alone: the rest stays byte for byte.
    let response = tgin
        .get(&format!("/bot{TOKEN}/getFile?file_id=colon"), &[])
        .await;
    let body = response.bytes().await.expect("body");
    let answered = serde_json::to_vec(&get_file_answer(&format!(
        "/srv/tba/{TOKEN}/documents/file_1.pdf"
    )))
    .expect("JSON");
    let answered = String::from_utf8(answered).expect("UTF-8");
    let expected = answered.replace(&format!("/srv/tba/{TOKEN}/"), "");
    assert_eq!(std::str::from_utf8(&body).expect("UTF-8"), expected);

    let response = tgin
        .get(&format!("/bot{TOKEN}/getFile?file_id=failed"), &[])
        .await;
    assert_eq!(response.status(), 400);
    let value: Value =
        serde_json::from_slice(&response.bytes().await.expect("body")).expect("JSON");
    assert_eq!(value, failed);

    let response = tgin
        .get(
            &format!("/bot{TOKEN}/getStickerSet"),
            &[("accept-encoding", "gzip")],
        )
        .await;
    let value: Value =
        serde_json::from_slice(&response.bytes().await.expect("body")).expect("JSON");
    assert_eq!(value, other_method);

    let response = tgin
        .get(&format!("/bot{TOKEN}/getFile?file_id=large"), &[])
        .await;
    assert_eq!(response.status(), 200);
    let value: Value =
        serde_json::from_slice(&response.bytes().await.expect("body")).expect("JSON");
    assert_eq!(value, large);

    // A rewritten getFile goes upstream uncompressed (its answer is read);
    // a verbatim one, and any other method, keeps the client's header.
    let requests = server.received_requests().await.expect("recording");
    let encoding_of = |path: String| {
        let found: Vec<Option<String>> = requests
            .iter()
            .filter(|request| request.url.path() == path)
            .map(|request| {
                request
                    .headers
                    .get("accept-encoding")
                    .map(|value| value.to_str().expect("ASCII").to_owned())
            })
            .collect();
        assert!(!found.is_empty(), "no request at {path}");
        found
    };
    for token in [TOKEN, THIRD_TOKEN] {
        for encoding in encoding_of(format!("/bot{token}/getFile")) {
            assert_eq!(encoding, None);
        }
    }
    for token in [OTHER_TOKEN, FOURTH_TOKEN] {
        for encoding in encoding_of(format!("/bot{token}/getFile")) {
            assert_eq!(encoding.as_deref(), Some("gzip"));
        }
    }
    assert_eq!(
        encoding_of(format!("/bot{TOKEN}/getStickerSet")),
        [Some("gzip".to_owned())]
    );

    tgin.stop().await;
}

/// Each token's '~' form, the directory name telegram-bot-api may use,
/// is a secret the logs mask, as the token itself is.
#[test]
fn token_directory_forms_are_secrets() {
    let yaml = format!(
        "version: 1\nserver: {{}}\nupstreams:\n  tba: {{ kind: local, url: \"http://127.0.0.1:8081\" }}\nbots:\n  - name: a\n    token: \"{TOKEN}\"\n    upstream: tba\n    routes: [{{ to: [w] }}]\n  - name: b\n    token: \"{OTHER_TOKEN}\"\n    routes: [{{ to: [v] }}]\nconsumers:\n  w: {{ pull: {{}} }}\n  v: {{ pull: {{}} }}\n"
    );
    let cfg = tgin_core::config::load(&yaml, Path::new("tgin.yaml"), &MapEnv::default())
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    let secrets = secrets(&cfg);
    for token in [TOKEN, OTHER_TOKEN] {
        assert!(secrets.iter().any(|s| s == token));
        assert!(secrets.iter().any(|s| *s == tilde(token)));
    }
}

/// The answer to a local file reference tgin refuses.
const LOCAL_REFUSED: &str = "Bad Request: local file uploads are disabled by tgin";
/// A local file reference; the logs never show its path.
const LOCAL_FILE: &str = "file:///srv/secret/passwd";
const SECRET_PATH: &str = "secret/passwd";
const JSON: &str = "application/json";
const FORM: &str = "application/x-www-form-urlencoded";
const BOUNDARY: &str = "local-files-boundary";
const MULTIPART: &str = "multipart/form-data; boundary=local-files-boundary";

/// Bots on `api`: `gw` (`TOKEN`) and `allowed` (`OTHER_TOKEN`, with
/// `allow_local_files`) on a local upstream in local mode, `plain`
/// (`THIRD_TOKEN`) on a local upstream without it, `cloud` (`FOURTH_TOKEN`)
/// on the cloud. Only `gw` keeps the default rate limits.
fn local_files_config(api: &str) -> String {
    format!(
        r#"version: 1
runtime: {{ lame_duck: 0s, shutdown_grace: 2s }}
server: {{ listen: ["127.0.0.1:0"] }}
admin: {{ listen: ["127.0.0.1:0"] }}
upstreams:
  tba: {{ kind: local, url: "{api}", local_mode: true }}
  plain: {{ kind: local, url: "{api}" }}
bots:
  - name: gw
    token: "{TOKEN}"
    upstream: tba
    routes: [{{ to: [a] }}]
  - name: allowed
    token: "{OTHER_TOKEN}"
    upstream: tba
    allow_local_files: true
    gateway: {{ rate_limit: {{ mode: off }} }}
    routes: [{{ to: [b] }}]
  - name: plain
    token: "{THIRD_TOKEN}"
    upstream: plain
    gateway: {{ rate_limit: {{ mode: off }} }}
    routes: [{{ to: [c] }}]
  - name: cloud
    token: "{FOURTH_TOKEN}"
    api_url: "{api}"
    gateway: {{ rate_limit: {{ mode: off }} }}
    routes: [{{ to: [d] }}]
consumers:
  a: {{ pull: {{}} }}
  b: {{ pull: {{}} }}
  c: {{ pull: {{}} }}
  d: {{ pull: {{}} }}
"#
    )
}

/// A server that answers every call with `{"ok":true,"result":true}`.
async fn answering_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"ok":true,"result":true}"#, JSON))
        .mount(&server)
        .await;
    server
}

/// Every byte but ASCII letters and digits percent-encoded.
fn encoded(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A multipart body of `parts` (header lines, value) with [`BOUNDARY`].
fn multipart(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (headers, value) in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n{headers}\r\n\r\n").as_bytes());
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// The headers of the text field `name`.
fn text_field(name: &str) -> String {
    format!("Content-Disposition: form-data; name=\"{name}\"")
}

/// The headers of the file part `name`.
fn file_part(name: &str) -> String {
    format!(
        "Content-Disposition: form-data; name=\"{name}\"; filename=\"a.bin\"\r\nContent-Type: application/octet-stream"
    )
}

/// Requests carrying [`LOCAL_FILE`] in every position a server in local
/// mode reads it from: (method, content type, body).
fn local_references() -> Vec<(&'static str, &'static str, Vec<u8>)> {
    let media = format!(r#"[{{"type":"photo","media":"{LOCAL_FILE}"}}]"#);
    let json = |body: String| body.into_bytes();
    vec![
        (
            "sendDocument",
            JSON,
            json(format!(r#"{{"chat_id":1,"document":"{LOCAL_FILE}"}}"#)),
        ),
        // JSON escapes decode to the same reference.
        (
            "sendDocument",
            JSON,
            json(r#"{"chat_id":1,"document":"file:\/\/\/srv\/secret\/passwd"}"#.to_owned()),
        ),
        (
            "sendDocument",
            JSON,
            json(r#"{"chat_id":1,"document":"\u0066ile:///srv/secret/passwd"}"#.to_owned()),
        ),
        (
            "sendDocument",
            JSON,
            json(format!(
                r#"{{"chat_id":1,"document":"BQACAgIAAx0","thumbnail":"{LOCAL_FILE}"}}"#
            )),
        ),
        (
            "sendMediaGroup",
            JSON,
            json(format!(r#"{{"chat_id":1,"media":{media}}}"#)),
        ),
        // JSON carried in a string parameter.
        (
            "sendMediaGroup",
            JSON,
            json(format!(
                r#"{{"chat_id":1,"media":{}}}"#,
                serde_json::to_string(&media).expect("JSON")
            )),
        ),
        (
            "sendMediaGroup",
            FORM,
            json(format!("chat_id=1&media={}", encoded(&media))),
        ),
        (
            "sendDocument",
            FORM,
            json(format!("chat_id=1&document={}", encoded(LOCAL_FILE))),
        ),
        (
            "sendMediaGroup",
            MULTIPART,
            multipart(&[
                (&text_field("chat_id"), b"1"),
                (&text_field("media"), media.as_bytes()),
            ]),
        ),
        // A text field after a file part.
        (
            "sendDocument",
            MULTIPART,
            multipart(&[
                (&text_field("chat_id"), b"1"),
                (&file_part("document"), b"\x89PNG\r\n--not-the-boundary"),
                (&text_field("thumbnail"), LOCAL_FILE.as_bytes()),
            ]),
        ),
        // A part the server reads as urlencoded parameters.
        (
            "sendDocument",
            MULTIPART,
            multipart(&[
                (&text_field("chat_id"), b"1"),
                (
                    &format!(
                        "{}\r\nContent-Type: application/x-www-form-urlencoded",
                        text_field("params")
                    ),
                    format!("document={}", encoded(LOCAL_FILE)).as_bytes(),
                ),
            ]),
        ),
        // The server sees `filename` inside a quoted parameter, so no file
        // name: a text field.
        (
            "sendDocument",
            MULTIPART,
            multipart(&[
                (&text_field("chat_id"), b"1"),
                (
                    "Content-Disposition: form-data; name=thumbnail; x=\"; filename=a\"",
                    LOCAL_FILE.as_bytes(),
                ),
            ]),
        ),
    ]
}

/// Query strings carrying [`LOCAL_FILE`]: (HTTP method, path and query,
/// content type, body).
fn query_references() -> Vec<(reqwest::Method, String, &'static str, Vec<u8>)> {
    vec![
        (
            reqwest::Method::POST,
            format!("sendDocument?chat_id=1&document={}", encoded(LOCAL_FILE)),
            "",
            Vec::new(),
        ),
        (
            reqwest::Method::POST,
            format!("sendDocument?document={LOCAL_FILE}"),
            JSON,
            br#"{"chat_id":1}"#.to_vec(),
        ),
        (
            reqwest::Method::POST,
            format!("sendDocument?thumbnail={LOCAL_FILE}"),
            MULTIPART,
            multipart(&[(&text_field("chat_id"), b"1")]),
        ),
        (
            reqwest::Method::GET,
            "sendPhoto?chat_id=1&photo=%66ile:///srv/secret/passwd".to_owned(),
            "",
            Vec::new(),
        ),
    ]
}

/// Asserts that the last request `server` received went to `token`'s
/// `method` with `body`, byte for byte.
async fn assert_forwarded(server: &MockServer, token: &str, method: &str, body: &[u8]) {
    let requests = server.received_requests().await.expect("recording");
    let last = requests.last().expect("a request");
    assert_eq!(last.url.path(), format!("/bot{token}/{method}"));
    assert_eq!(last.body, body, "{method}");
}

/// A JSON body over 1 MiB.
fn large_json() -> Vec<u8> {
    format!(r#"{{"chat_id":4,"text":"{}"}}"#, "x".repeat(1 << 20)).into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_file_refused_in_every_position() {
    let server = answering_server().await;
    let tgin = start(&local_files_config(&server.uri())).await;

    for (method, content_type, body) in local_references() {
        let response = tgin
            .send(
                reqwest::Method::POST,
                &format!("/bot{TOKEN}/{method}"),
                content_type,
                body.clone(),
            )
            .await;
        assert_eq!(
            description(response).await,
            (400, LOCAL_REFUSED.to_owned()),
            "{method} {}",
            String::from_utf8_lossy(&body)
        );
    }
    let large = tgin
        .send(
            reqwest::Method::POST,
            &format!("/bot{TOKEN}/sendMessage"),
            JSON,
            large_json(),
        )
        .await;
    assert_eq!(
        description(large).await,
        (413, "Request Entity Too Large".to_owned())
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recording")
            .is_empty(),
        "a refused call reached the server"
    );

    // "file:" anywhere but at a value's start, and a file part's bytes, are
    // no references.
    let text = br#"{"chat_id":2,"text":"see file:///srv/secret/passwd"}"#;
    let response = tgin
        .send(
            reqwest::Method::POST,
            &format!("/bot{TOKEN}/sendMessage"),
            JSON,
            &text[..],
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_forwarded(&server, TOKEN, "sendMessage", text).await;
    let upload = multipart(&[
        (&text_field("chat_id"), b"3"),
        (&file_part("document"), LOCAL_FILE.as_bytes()),
    ]);
    let response = tgin
        .send(
            reqwest::Method::POST,
            &format!("/bot{TOKEN}/sendDocument"),
            MULTIPART,
            upload.clone(),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_forwarded(&server, TOKEN, "sendDocument", &upload).await;

    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendDocument",outcome="rejected"} 8"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMediaGroup",outcome="rejected"} 4"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendMessage",outcome="rejected"} 1"#,
    );
    assert!(
        !LOGS.text().contains(SECRET_PATH),
        "a path reached the logs"
    );
    assert_logs_clean(&[TOKEN]);
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_file_in_query_string_refused() {
    let server = answering_server().await;
    let tgin = start(&local_files_config(&server.uri())).await;

    for (method, path, content_type, body) in query_references() {
        let response = tgin
            .send(method, &format!("/bot{TOKEN}/{path}"), content_type, body)
            .await;
        assert_eq!(
            description(response).await,
            (400, LOCAL_REFUSED.to_owned()),
            "{path}"
        );
    }
    assert!(
        server
            .received_requests()
            .await
            .expect("recording")
            .is_empty(),
        "a refused call reached the server"
    );

    let response = tgin
        .get(&format!("/bot{TOKEN}/getChat?chat_id=1&title=file"), &[])
        .await;
    assert_eq!(response.status(), 200);
    let requests = server.received_requests().await.expect("recording");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.query(), Some("chat_id=1&title=file"));
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendDocument",outcome="rejected"} 3"#,
    );
    tgin.assert_metric(
        r#"tgin_gateway_requests_total{bot="gw",method="sendPhoto",outcome="rejected"} 1"#,
    );
    assert!(
        !LOGS.text().contains(SECRET_PATH),
        "a path reached the logs"
    );
    tgin.stop().await;
}

/// Sends every reference of [`local_references`] and
/// [`query_references`] for `token` and asserts each reached the server
/// as it was sent.
async fn assert_references_pass(tgin: &Tgin, server: &MockServer, token: &str) {
    for (method, content_type, body) in local_references() {
        let response = tgin
            .send(
                reqwest::Method::POST,
                &format!("/bot{token}/{method}"),
                content_type,
                body.clone(),
            )
            .await;
        assert_eq!(response.status(), 200, "{method}");
        assert_forwarded(server, token, method, &body).await;
    }
    for (method, path, content_type, body) in query_references() {
        let response = tgin
            .send(method, &format!("/bot{token}/{path}"), content_type, body)
            .await;
        assert_eq!(response.status(), 200, "{path}");
        let requests = server.received_requests().await.expect("recording");
        let last = requests.last().expect("a request");
        let query = path.split_once('?').expect("a query").1;
        assert_eq!(last.url.query(), Some(query));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allowed_bot_passes_file_references() {
    let server = answering_server().await;
    let tgin = start(&local_files_config(&server.uri())).await;
    assert_references_pass(&tgin, &server, OTHER_TOKEN).await;
    tgin.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloud_and_non_local_upstreams_unaffected() {
    let server = answering_server().await;
    let tgin = start(&local_files_config(&server.uri())).await;
    for token in [THIRD_TOKEN, FOURTH_TOKEN] {
        assert_references_pass(&tgin, &server, token).await;
        // No 1 MiB bound either.
        let large = large_json();
        let response = tgin
            .send(
                reqwest::Method::POST,
                &format!("/bot{token}/sendMessage"),
                JSON,
                large.clone(),
            )
            .await;
        assert_eq!(response.status(), 200);
        assert_forwarded(&server, token, "sendMessage", &large).await;
    }
    tgin.stop().await;
}
