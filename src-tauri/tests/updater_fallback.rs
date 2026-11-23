//! 多个升级接口的兜底（`desktop::update::check_in_order`、`download_with_fallback`）：用模拟
//! 运行时和本机假服务器跑 tauri-plugin-updater 真实的检查、下载和签名校验流程。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lubobao_lib::desktop::update::{
    Found, Progress, UpdateError, UpdateErrorKind, check_in_order, download_with_fallback,
};
use tauri::test::MockRuntime;

/// 只用于测试的密钥对（`tauri signer generate`，私钥没有保存）的公钥。
const PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEIxRTc3NkU1OEE3NEJEQzAKUldUQXZYU0s1WGJuc1N3MFduRHhOZjY5NU5nM1V0UVpJZnc1cHoxeFZ3aDZFSHNTUWI4WEFtNjYK";
/// 假安装包。
const PACKAGE: &[u8] = b"lubobao test update package 99.0.0\n";
/// `tauri signer sign --app-version 99.0.0` 对 [`PACKAGE`] 的签名（可信注释里有 `version:99.0.0`）。
const PACKAGE_SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVUQXZYU0s1WGJuc1M0aW45alhxWDV0U2VIeUZMREVuNjlQL3R6emd5SFh3MGsvUEVuQnFJdE96czRvMlk5d09HMU1Cc0FhdlRtLzMvU3BFRkFNQzllZHgyOEhrNTl3aWdjPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzk1NDYzMTI0CWZpbGU6cGtnLmJpbgl2ZXJzaW9uOjk5LjAuMApqTEtKYXUxWjhueCtoRTN4VzhES1U5YXZ1MFdXNHpiSWYvMmlZaHZQeC9abG9mS3BrdjRkUWtNcitxZ1E5MEV0WkUveUludEFBYVlmdDBTT0V6bHpCZz09Cg==";

/// 本机 HTTP 服务器：每个请求记下请求头（小写），回复 `response`；`None` 时连上后不回复。
struct FakeServer {
    url: tauri::Url,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FakeServer {
    fn start(response: Option<String>) -> Self {
        Self::start_bytes(response.map(String::into_bytes))
    }

    fn start_bytes(response: Option<Vec<u8>>) -> Self {
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
                        let _ = stream.write_all(response);
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

/// 安装包下载的回复。
fn http_bytes(status: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

/// 与 scripts/updater-latest-json.mjs 生成的 latest.json 同样的结构。
fn static_manifest() -> String {
    manifest("99.0.0", "c2lnbmF0dXJl", |name| {
        format!("https://github.com/o/r/releases/download/v99.0.0/{name}")
    })
}

/// 静态格式的清单：`url` 由安装包文件名得出下载地址。
fn manifest(version: &str, signature: &str, url: impl Fn(&str) -> String) -> String {
    let platform = |name: &str| {
        serde_json::json!({
            "signature": signature,
            "url": url(name),
        })
    };
    serde_json::json!({
        "version": version,
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
fn check(endpoints: Vec<tauri::Url>, timeout: Duration) -> Result<Option<Found>, UpdateError> {
    let app = mock_app();
    tauri::async_runtime::block_on(check_in_order(app.handle(), endpoints, false, timeout))
}

#[test]
fn falls_back_to_the_static_manifest_when_the_first_endpoint_errors() {
    let broken = FakeServer::start(Some(http("500 Internal Server Error", "")));
    let github = FakeServer::start(Some(http("200 OK", &static_manifest())));
    let Ok(Some(Found { update, fallbacks })) = check(
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
    // 最后一个接口给出的更新，下载时没有更多的备用线路。
    assert!(fallbacks.is_empty());
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
    let Ok(Some(Found { update, fallbacks })) = check(
        vec![hanging.url.clone(), closed, github.url.clone()],
        Duration::from_secs(1),
    ) else {
        panic!("the last endpoint answers");
    };
    assert_eq!(update.version, "99.0.0");
    assert!(fallbacks.is_empty());
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

// ---- 下载兜底（download_with_fallback） ----

/// 下载结果：实际下载的那份更新和安装包。
type Downloaded = Result<(Found, Vec<u8>), UpdateError>;

/// 以发布构建的规则（`debug = false`）先查 `xiaofany`、再查 `github`，然后下载查到的更新。
/// 返回下载结果和下载期间的全部进度。
fn check_and_download(xiaofany: &FakeServer, github: &FakeServer) -> (Downloaded, Vec<Progress>) {
    let app = mock_app();
    let timeout = Duration::from_secs(5);
    let endpoints = vec![xiaofany.url.clone(), github.url.clone()];
    tauri::async_runtime::block_on(async {
        let found = check_in_order(app.handle(), endpoints, false, timeout)
            .await
            .unwrap_or_else(|error| panic!("check failed: {}", error.message))
            .expect("xiaofany announces 99.0.0");
        assert_eq!(found.fallbacks, vec![github.url.clone()]);
        let mut progress = Vec::new();
        let result =
            download_with_fallback(app.handle(), found, false, timeout, |p| progress.push(p)).await;
        (result, progress)
    })
}

/// 清单服务器，宣布 `version`，下载地址指向 `download`。
fn manifest_server(version: &str, signature: &str, download: &FakeServer) -> FakeServer {
    let url = download.url.clone();
    let body = manifest(version, signature, |name| format!("{url}/{name}"));
    FakeServer::start(Some(http("200 OK", &body)))
}

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "release builds only download over https"
)]
fn downloads_the_same_version_from_github_when_the_xiaofany_download_fails() {
    // 500，以及声明了长度却中途断开的下载。
    let broken_downloads = [http_bytes("500 Internal Server Error", b""), {
        let mut cut = http_bytes("200 OK", PACKAGE);
        cut.truncate(cut.len() - 10);
        cut
    }];
    for broken in broken_downloads {
        let r2 = FakeServer::start_bytes(Some(broken));
        let release = FakeServer::start_bytes(Some(http_bytes("200 OK", PACKAGE)));
        let xiaofany = manifest_server("99.0.0", PACKAGE_SIGNATURE, &r2);
        let github = manifest_server("99.0.0", PACKAGE_SIGNATURE, &release);

        let (result, progress) = check_and_download(&xiaofany, &github);
        let (found, bytes) = result.unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(bytes, PACKAGE);
        assert_eq!(found.update.version, "99.0.0");
        assert!(
            found
                .update
                .download_url
                .as_str()
                .starts_with(release.url.as_str())
        );
        assert!(found.fallbacks.is_empty());
        assert_eq!(r2.requests().len(), 1);
        assert_eq!(github.requests().len(), 1);
        assert_eq!(release.requests().len(), 1);
        // 发给 GitHub 的清单和下载请求都不带客户端 ID。
        for request in github.requests().iter().chain(release.requests().iter()) {
            assert!(!request.contains("x-client-id"), "{request}");
        }
        // 换线路时先报一次「从 0 开始」，之后的进度都标着备用线路，最后下完。
        let switch = progress
            .iter()
            .position(|p| p.fallback)
            .expect("the switch to the fallback is reported");
        assert_eq!(progress[switch].downloaded, 0);
        assert!(progress[switch..].iter().all(|p| p.fallback));
        assert_eq!(progress.last().unwrap().downloaded, PACKAGE.len() as u64);
    }
}

#[test]
fn does_not_download_a_different_version_from_github() {
    let r2 = FakeServer::start_bytes(Some(http_bytes("500 Internal Server Error", b"")));
    let release = FakeServer::start_bytes(Some(http_bytes("200 OK", PACKAGE)));
    let xiaofany = manifest_server("99.0.0", PACKAGE_SIGNATURE, &r2);
    // GitHub 上已经是更新的版本（或还没发布这个版本）。
    let github = manifest_server("99.0.1", PACKAGE_SIGNATURE, &release);

    let (result, _) = check_and_download(&xiaofany, &github);
    let Err(error) = result else {
        panic!("a different version must not be downloaded");
    };
    assert_eq!(error.kind, UpdateErrorKind::Network);
    // 报的是原来的下载失败。
    assert!(error.message.contains("500"), "{}", error.message);
    assert_eq!(github.requests().len(), 1);
    assert!(release.requests().is_empty());
}

#[test]
fn a_bad_signature_fails_without_trying_github() {
    let tampered = b"lubobao test update package 99.0.0 (tampered)\n";
    let r2 = FakeServer::start_bytes(Some(http_bytes("200 OK", tampered)));
    let release = FakeServer::start_bytes(Some(http_bytes("200 OK", PACKAGE)));
    let xiaofany = manifest_server("99.0.0", PACKAGE_SIGNATURE, &r2);
    let github = manifest_server("99.0.0", PACKAGE_SIGNATURE, &release);

    let (result, progress) = check_and_download(&xiaofany, &github);
    let Err(error) = result else {
        panic!("a package that does not match its signature must be rejected");
    };
    assert_eq!(error.kind, UpdateErrorKind::Signature);
    assert_eq!(r2.requests().len(), 1);
    assert!(github.requests().is_empty());
    assert!(release.requests().is_empty());
    assert!(progress.iter().all(|p| !p.fallback));
}

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "release builds only download over https"
)]
fn a_bad_signature_on_the_fallback_is_not_installed() {
    let r2 = FakeServer::start_bytes(Some(http_bytes("502 Bad Gateway", b"")));
    let release = FakeServer::start_bytes(Some(http_bytes("200 OK", b"not the signed package")));
    let xiaofany = manifest_server("99.0.0", PACKAGE_SIGNATURE, &r2);
    let github = manifest_server("99.0.0", PACKAGE_SIGNATURE, &release);

    let (result, _) = check_and_download(&xiaofany, &github);
    let Err(error) = result else {
        panic!("the fallback package does not match its signature");
    };
    assert_eq!(error.kind, UpdateErrorKind::Signature);
    assert_eq!(release.requests().len(), 1);
}
