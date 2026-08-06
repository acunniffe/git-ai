use crate::repos::test_repo::TestRepo;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

const RELEASES_RESPONSE: &str =
    r#"{"channels":{"latest":{"version":"0.0.0","checksum":"unused"}}}"#;

struct OneShotHttpServer {
    address: SocketAddr,
    request: Receiver<String>,
}

fn read_http_headers(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 1024];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    bytes
}

impl OneShotHttpServer {
    fn start(response_body: impl AsRef<[u8]>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request) = mpsc::channel();
        let response_body = response_body.as_ref().to_vec();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();

            let mut bytes = read_http_headers(&mut stream);
            if bytes.starts_with(b"CONNECT ") {
                stream
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .unwrap();
                stream.flush().unwrap();
                bytes.extend_from_slice(&read_http_headers(&mut stream));
            }

            request_tx
                .send(String::from_utf8_lossy(&bytes).into_owned())
                .unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            )
            .unwrap();
            stream.write_all(&response_body).unwrap();
        });

        Self { address, request }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn received_request(&self) -> String {
        self.request
            .recv_timeout(Duration::from_secs(5))
            .expect("HTTP server did not receive a request")
    }
}

fn proxy_env<'a>(proxy_url: &'a str, no_proxy: &'a str) -> [(&'a str, &'a str); 10] {
    // Windows environment variable names are case-insensitive, so setting the
    // uppercase variant to empty would also clear the lowercase variant.
    let (http_proxy, uppercase_http_proxy) = if cfg!(windows) {
        ("", proxy_url)
    } else {
        (proxy_url, "")
    };

    [
        ("http_proxy", http_proxy),
        ("HTTP_PROXY", uppercase_http_proxy),
        ("https_proxy", ""),
        ("HTTPS_PROXY", ""),
        ("all_proxy", ""),
        ("ALL_PROXY", ""),
        ("no_proxy", no_proxy),
        ("NO_PROXY", no_proxy),
        ("GIT_AI_API_BASE_URL", "http://git-ai-proxy-test.invalid"),
        ("GIT_AI_DISABLE_VERSION_CHECKS", "false"),
    ]
}

#[test]
fn upgrade_uses_http_proxy_from_environment() {
    let repo = TestRepo::new();
    let proxy = OneShotHttpServer::start(RELEASES_RESPONSE);
    let proxy_url = proxy.url();

    let output = repo.git_ai_with_env(&["upgrade"], &proxy_env(&proxy_url, ""));
    let request = proxy.received_request();

    assert!(
        output
            .as_ref()
            .is_ok_and(|output| output.contains("You are running a newer version")),
        "upgrade output: {output:?}; proxy request: {request:?}"
    );
    assert!(
        request.starts_with("CONNECT git-ai-proxy-test.invalid:80 HTTP/1.1")
            && request.contains("GET /worker/releases HTTP/1.1"),
        "unexpected proxy request: {request:?}"
    );
}

#[test]
fn upgrade_respects_no_proxy_from_environment() {
    let repo = TestRepo::new();
    let target = OneShotHttpServer::start(RELEASES_RESPONSE);
    let unavailable_proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_url = format!("http://{}", unavailable_proxy.local_addr().unwrap());
    drop(unavailable_proxy);
    let target_url = target.url();
    let mut env = proxy_env(&proxy_url, "127.0.0.1");
    env[8] = ("GIT_AI_API_BASE_URL", &target_url);

    let output = repo.git_ai_with_env(&["upgrade"], &env).unwrap();

    assert!(output.contains("You are running a newer version"));
    assert!(
        target
            .received_request()
            .starts_with("GET /worker/releases ")
    );
}

#[test]
fn upgrade_accepts_response_larger_than_ureq_default_limit() {
    let repo = TestRepo::new();
    let mut response = RELEASES_RESPONSE.as_bytes().to_vec();
    response.resize(11 * 1024 * 1024, b' ');
    let target = OneShotHttpServer::start(response);
    let target_url = target.url();
    let mut env = proxy_env("", "");
    env[8] = ("GIT_AI_API_BASE_URL", &target_url);

    let output = repo.git_ai_with_env(&["upgrade"], &env).unwrap();

    assert!(output.contains("You are running a newer version"));
    assert!(
        target
            .received_request()
            .starts_with("GET /worker/releases ")
    );
}
