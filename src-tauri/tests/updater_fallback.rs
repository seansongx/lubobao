//! 多个升级接口的兜底（`desktop::update::check_in_order`）：用模拟运行时和本机假服务器跑
//! tauri-plugin-updater 真实的检查流程。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lubobao_lib::desktop::update::{UpdateError, UpdateErrorKind, check_in_order};
use tauri::test::MockRuntime;
use tauri_plugin_updater::Update;

const PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDQwMzVFMTYzRDBEMjU2MDIKUldRQ1Z0TFFZK0UxUURzVWR2VU4rM2I5Y2RiS3ZNazJNK05mNyt6eTg4dEpGQzFFWE1YVHhFaDkK";

/// 本机 HTTP 服务器：每个请求记下请求头（小写），回复 `response`；`None` 时连上后不回复。
struct FakeServer {
    url: tauri::Url,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FakeServer {
    fn start(response: Option<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/update", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                    }
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buffer).to_ascii_lowercase());
                match &response {
                    Some(response) => {
                        let _ = stream.write_all(response.as_bytes());
                    }
                    None => std::thread::sleep(Duration::from_secs(10)),
                }
            }
        });
        Self {
            url: tauri::Url::parse(&url).unwrap(),
            requests,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn http(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// 与 scripts/updater-latest-json.mjs 生成的 latest.json 同样的结构。
fn static_manifest() -> String {
    let platform = |name: &str| {
        serde_json::json!({
            "signature": "c2lnbmF0dXJl",
            "url": format!("https://github.com/o/r/releases/download/v99.0.0/{name}"),
        })
    };
    serde_json::json!({
        "version": "99.0.0",
        "notes": "- 修复",
        "pub_date": "2026-11-23T08:00:00Z",
        "platforms": {
            "windows-x86_64": platform("lubobao_99.0.0_windows_x64-setup.exe"),
            "darwin-aarch64": platform("lubobao_99.0.0_macos_arm64.app.tar.gz"),
            "darwin-x86_64": platform("lubobao_99.0.0_macos_x64.app.tar.gz"),
            "linux-x86_64": platform("lubobao_99.0.0_linux_amd64.AppImage"),
        },
    })
    .to_string()
}

fn mock_app() -> tauri::App<MockRuntime> {
    let mut context = tauri::test::mock_context(tauri::test::noop_assets());
    context.config_mut().plugins.0.insert(
        "updater".into(),
        serde_json::json!({ "pubkey": PUBKEY, "requireSignedVersion": true }),
    );
    tauri::test::mock_builder()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .build(context)
        .unwrap()
}

/// 以发布构建的规则检查（`debug = false`：本机地址不收客户端 ID）。
fn check(endpoints: Vec<tauri::Url>, timeout: Duration) -> Result<Option<Update>, UpdateError> {
    let app = mock_app();
    tauri::async_runtime::block_on(check_in_order(app.handle(), endpoints, false, timeout))
}

#[test]
fn falls_back_to_the_static_manifest_when_the_first_endpoint_errors() {
    let broken = FakeServer::start(Some(http("500 Internal Server Error", "")));
    let github = FakeServer::start(Some(http("200 OK", &static_manifest())));
    let Ok(Some(update)) = check(
        vec![broken.url.clone(), github.url.clone()],
        Duration::from_secs(10),
    ) else {
        panic!("the static manifest announces a newer version");
    };
    assert_eq!(update.version, "99.0.0");
    assert_eq!(update.body.as_deref(), Some("- 修复"));
    assert_eq!(update.signature, "c2lnbmF0dXJl");
    assert!(
        update
            .download_url
            .as_str()
            .starts_with("https://github.com/o/r/releases/download/v99.0.0/lubobao_99.0.0_")
    );
    assert_eq!(broken.requests().len(), 1);
    assert_eq!(github.requests().len(), 1);
    // 非 xiaofany.com 的接口不带客户端 ID，检查得到的更新也不会带给下载服务器。
    assert!(github.requests().iter().all(|r| !r.contains("x-client-id")));
    assert!(!update.headers.contains_key("X-Client-Id"));
}

#[test]
fn falls_back_when_the_first_endpoint_times_out_or_is_unreachable() {
    let hanging = FakeServer::start(None);
    let github = FakeServer::start(Some(http("200 OK", &static_manifest())));
    // 没有服务的端口：连接被拒绝。
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        tauri::Url::parse(&format!("http://{addr}/update")).unwrap()
    };
    let started = Instant::now();
    let Ok(Some(update)) = check(
        vec![hanging.url.clone(), closed, github.url.clone()],
        Duration::from_secs(1),
    ) else {
        panic!("the last endpoint answers");
    };
    assert_eq!(update.version, "99.0.0");
    assert_eq!(hanging.requests().len(), 1);
    assert_eq!(github.requests().len(), 1);
    // 时限按接口分别计时：卡住的接口 1 秒后放弃，不会一直等。
    assert!(started.elapsed() < Duration::from_secs(8));
}

#[test]
fn no_content_from_the_first_endpoint_means_up_to_date_without_fallback() {
    let xiaofany = FakeServer::start(Some(http("204 No Content", "")));
    let github = FakeServer::start(Some(http("200 OK", &static_manifest())));
    let found = check(
        vec![xiaofany.url.clone(), github.url.clone()],
        Duration::from_secs(10),
    );
    assert!(matches!(found, Ok(None)));
    assert!(github.requests().is_empty());
}

#[test]
fn reports_the_last_error_when_every_endpoint_fails() {
    let first = FakeServer::start(Some(http("500 Internal Server Error", "")));
    let second = FakeServer::start(Some(http("404 Not Found", "")));
    let Err(error) = check(
        vec![first.url.clone(), second.url.clone()],
        Duration::from_secs(10),
    ) else {
        panic!("every endpoint failed, check must fail");
    };
    assert_eq!(error.kind, UpdateErrorKind::Network);
    assert_eq!(first.requests().len(), 1);
    assert_eq!(second.requests().len(), 1);
}
