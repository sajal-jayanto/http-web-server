//! Tests for config parsing, routing, load balancing, rate limiting and health checks.
//!
//! Unit tests call the internals directly. End-to-end tests start the real proxy
//! (from a YAML string) in a background thread, in front of tiny mock upstreams.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use super::*;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn parse(yaml: &str) -> Config {
    serde_yaml::from_str(yaml).expect("config should parse")
}

fn route(yaml: &str) -> RouteConfig {
    serde_yaml::from_str(yaml).expect("route should parse")
}

/// Build a balancer (no health check) for the given algorithm and upstreams
fn balancer(algorithm: &str, upstreams: &[&str]) -> Balancer {
    let route = route(&format!(
        "algorithm: {algorithm}\nupstreams: [{}]",
        upstreams.join(", ")
    ));
    let mut server = Server::new_with_opt_and_conf(Opt::default(), ServerConf::default());
    Balancer::new("test", &route, &mut server).unwrap()
}

fn pick(balancer: &Balancer, key: &str) -> String {
    balancer.select(key.as_bytes()).unwrap().addr.to_string()
}

/// A port that is free right now
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Mock upstream: answers every request with 200 and its own `name` as the body.
/// If `healthy` is false, `/health` answers 500 instead.
fn mock_upstream(name: &'static str, healthy: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || serve_mock(stream, name, healthy));
        }
    });
    addr
}

fn serve_mock(mut stream: TcpStream, name: &str, healthy: bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let request = String::from_utf8_lossy(&buf);
    let path = request.split_whitespace().nth(1).unwrap_or("/");
    let status = if path == "/health" && !healthy {
        "500 Internal Server Error"
    } else {
        "200 OK"
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{name}",
        name.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// An address where nothing is listening
fn dead_upstream() -> String {
    format!("127.0.0.1:{}", free_port())
}

/// Start the proxy from a YAML config and wait until every port accepts connections
fn start_proxy(yaml: String, ports: &[u16]) {
    thread::spawn(move || build_server(&yaml).run_forever());
    let deadline = Instant::now() + Duration::from_secs(10);
    for port in ports {
        while TcpStream::connect(("127.0.0.1", *port)).is_err() {
            assert!(
                Instant::now() < deadline,
                "proxy did not start on port {port}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Send `GET path` to the proxy; returns (status code, body)
fn get(port: u16, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("bad response: {response:?}"));
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    (status, body)
}

// ---------------------------------------------------------------------------
// config parsing
// ---------------------------------------------------------------------------

#[test]
fn config_full_route_parses() {
    let config = parse(
        "
clusters:
  - name: api
    listen: 0.0.0.0:8080
    routes:
      - path: /api
        algorithm: consistent
        rate_limit: { requests: 100, window_secs: 60 }
        health_check: { type: http, path: /health, interval_secs: 10 }
        upstreams: [127.0.0.1:3000, 127.0.0.1:3001]
",
    );
    let cluster = &config.clusters[0];
    assert_eq!(cluster.name, "api");
    assert_eq!(cluster.listen, "0.0.0.0:8080");

    let route = &cluster.routes[0];
    assert_eq!(route.path, "/api");
    assert!(matches!(route.algorithm, Algorithm::Consistent));
    assert_eq!(route.upstreams, ["127.0.0.1:3000", "127.0.0.1:3001"]);

    let limit = route.rate_limit.as_ref().unwrap();
    assert_eq!(limit.requests, 100);
    assert_eq!(limit.window_secs, 60);

    let hc = route.health_check.as_ref().unwrap();
    assert!(matches!(hc.kind, HealthCheckKind::Http));
    assert_eq!(hc.path, "/health");
    assert_eq!(hc.interval_secs, 10);
}

#[test]
fn config_listen_port_only_defaults_to_all_interfaces() {
    let config = parse(
        "
clusters:
  - { name: a, listen: 8080, routes: [] }
  - { name: b, listen: '9090', routes: [] }
  - { name: c, listen: 127.0.0.1:7070, routes: [] }
  - { name: d, listen: '[::1]:6060', routes: [] }
",
    );
    let listens: Vec<&str> = config.clusters.iter().map(|c| c.listen.as_str()).collect();
    assert_eq!(
        listens,
        [
            "0.0.0.0:8080",
            "0.0.0.0:9090",
            "127.0.0.1:7070",
            "[::1]:6060"
        ]
    );
}

#[test]
fn config_listen_rejects_bad_port() {
    let result: Result<Config, _> =
        serde_yaml::from_str("clusters: [{ name: a, listen: 70000, routes: [] }]");
    assert!(result.is_err(), "70000 is not a valid port");
}

#[test]
fn config_tls_and_http2_are_optional() {
    let config = parse(
        "
clusters:
  - { name: plain, listen: 8080, routes: [] }
  - name: secure
    listen: 8443
    http2: true
    tls: { cert: certs/server.crt, key: certs/server.key }
    routes: []
",
    );
    let (plain, secure) = (&config.clusters[0], &config.clusters[1]);
    assert!(plain.tls.is_none());
    assert!(!plain.http2, "HTTP/1.1 only by default");

    assert!(secure.http2);
    let tls = secure.tls.as_ref().unwrap();
    assert_eq!(tls.cert, "certs/server.crt");
    assert_eq!(tls.key, "certs/server.key");
}

#[test]
fn config_tls_requires_cert_and_key() {
    let result: Result<Config, _> = serde_yaml::from_str(
        "clusters: [{ name: a, listen: 8443, tls: { cert: a.crt }, routes: [] }]",
    );
    assert!(result.is_err(), "tls without key should be rejected");
}

#[test]
fn config_route_defaults() {
    let route = route("upstreams: [127.0.0.1:3000]");
    assert_eq!(route.path, "/");
    assert!(matches!(route.algorithm, Algorithm::RoundRobin));
    assert!(route.rate_limit.is_none());
    assert!(route.health_check.is_none());
}

#[test]
fn config_rate_limit_default_window_is_one_second() {
    let route = route("rate_limit: { requests: 5 }\nupstreams: [127.0.0.1:3000]");
    let limit = route.rate_limit.unwrap();
    assert_eq!(limit.requests, 5);
    assert_eq!(limit.window_secs, 1);
}

#[test]
fn config_health_check_defaults() {
    let route = route("health_check: {}\nupstreams: [127.0.0.1:3000]");
    let hc = route.health_check.unwrap();
    assert!(matches!(hc.kind, HealthCheckKind::Tcp));
    assert_eq!(hc.path, "/");
    assert_eq!(hc.interval_secs, 5);
}

#[test]
fn config_all_algorithms_parse() {
    for (name, check) in [
        (
            "round_robin",
            (|a| matches!(a, Algorithm::RoundRobin)) as fn(&Algorithm) -> bool,
        ),
        ("random", |a| matches!(a, Algorithm::Random)),
        ("hash", |a| matches!(a, Algorithm::Hash)),
        ("consistent", |a| matches!(a, Algorithm::Consistent)),
    ] {
        let route = route(&format!("algorithm: {name}\nupstreams: [127.0.0.1:3000]"));
        assert!(check(&route.algorithm), "algorithm {name}");
    }
}

#[test]
fn config_rejects_unknown_algorithm() {
    let result: Result<RouteConfig, _> =
        serde_yaml::from_str("algorithm: fastest\nupstreams: [127.0.0.1:3000]");
    assert!(result.is_err());
}

#[test]
fn config_rejects_unknown_health_check_type() {
    let result: Result<RouteConfig, _> =
        serde_yaml::from_str("health_check: { type: udp }\nupstreams: [127.0.0.1:3000]");
    assert!(result.is_err());
}

#[test]
fn config_requires_routes_and_upstreams() {
    let no_routes: Result<Config, _> =
        serde_yaml::from_str("clusters: [{ name: a, listen: 0.0.0.0:1 }]");
    assert!(no_routes.is_err());

    let no_upstreams: Result<RouteConfig, _> = serde_yaml::from_str("path: /api");
    assert!(no_upstreams.is_err());
}

#[test]
fn config_project_file_is_valid() {
    let raw = std::fs::read_to_string(CONFIG_PATH).unwrap();
    let config = parse(&raw);
    assert!(!config.clusters.is_empty());
}

// ---------------------------------------------------------------------------
// pingora server settings
// ---------------------------------------------------------------------------

#[test]
fn server_conf_reads_pingora_settings_and_ignores_app_keys() {
    let conf = server_conf("version: 1\nthreads: 7\nclusters: []");
    assert_eq!(conf.version, 1);
    assert_eq!(conf.threads, 7);
}

#[test]
fn server_conf_uses_our_default_paths() {
    let conf = server_conf("version: 1\nclusters: []");
    assert_eq!(conf.pid_file, DEFAULT_PID_FILE);
    assert_eq!(conf.upgrade_sock, DEFAULT_UPGRADE_SOCK);
}

#[test]
fn server_conf_file_overrides_default_paths() {
    let conf =
        server_conf("version: 1\npid_file: /tmp/a.pid\nupgrade_sock: /tmp/a.sock\nclusters: []");
    assert_eq!(conf.pid_file, "/tmp/a.pid");
    assert_eq!(conf.upgrade_sock, "/tmp/a.sock");
}

// ---------------------------------------------------------------------------
// path matching
// ---------------------------------------------------------------------------

#[test]
fn path_prefix_matching() {
    let cases = [
        ("/api", "/api", true),
        ("/api", "/api/", true),
        ("/api", "/api/users", true),
        ("/api", "/apis", false),
        ("/api", "/", false),
        ("/api", "/other/api", false),
        ("/api/", "/api/users", true),
        ("/api/", "/api", false),
        ("/", "/", true),
        ("/", "/anything/at/all", true),
    ];
    for (prefix, path, expected) in cases {
        assert_eq!(
            path_matches(prefix, path),
            expected,
            "prefix {prefix:?} vs path {path:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// load balancing algorithms
// ---------------------------------------------------------------------------

const UPSTREAMS: [&str; 3] = ["127.0.0.1:3000", "127.0.0.1:3001", "127.0.0.1:3002"];

#[test]
fn round_robin_cycles_through_all_upstreams() {
    let lb = balancer("round_robin", &UPSTREAMS);
    let picks: Vec<String> = (0..6).map(|_| pick(&lb, "")).collect();

    // every upstream used, and the pattern repeats every 3 requests
    assert_eq!(picks.iter().collect::<HashSet<_>>().len(), 3);
    assert_eq!(picks[0..3], picks[3..6]);
}

#[test]
fn random_only_picks_configured_upstreams() {
    let lb = balancer("random", &UPSTREAMS);
    let picks: HashSet<String> = (0..200).map(|_| pick(&lb, "")).collect();
    assert!(picks.iter().all(|p| UPSTREAMS.contains(&p.as_str())));
    assert!(picks.len() > 1, "random should use more than one upstream");
}

#[test]
fn hash_and_consistent_are_sticky_per_key() {
    for algorithm in ["hash", "consistent"] {
        let lb = balancer(algorithm, &UPSTREAMS);
        for client in ["10.0.0.1", "10.0.0.2", "192.168.1.50"] {
            let first = pick(&lb, client);
            for _ in 0..20 {
                assert_eq!(pick(&lb, client), first, "{algorithm}: {client} moved");
            }
        }
    }
}

#[test]
fn hash_and_consistent_spread_different_keys() {
    for algorithm in ["hash", "consistent"] {
        let lb = balancer(algorithm, &UPSTREAMS);
        let picks: HashSet<String> = (0..100)
            .map(|i| pick(&lb, &format!("10.0.0.{i}")))
            .collect();
        assert!(
            picks.len() > 1,
            "{algorithm} sent every client to one upstream"
        );
    }
}

#[test]
fn single_upstream_always_selected() {
    for algorithm in ["round_robin", "random", "hash", "consistent"] {
        let lb = balancer(algorithm, &["127.0.0.1:3000"]);
        assert_eq!(pick(&lb, "x"), "127.0.0.1:3000", "{algorithm}");
    }
}

// ---------------------------------------------------------------------------
// build_server validation
// ---------------------------------------------------------------------------

#[test]
#[should_panic(expected = "path already used on this listen")]
fn duplicate_path_on_same_listen_panics() {
    build_server(
        "
version: 1
clusters:
  - name: a
    listen: 127.0.0.1:1
    routes:
      - { path: /api, upstreams: [127.0.0.1:3000] }
  - name: b
    listen: 127.0.0.1:1
    routes:
      - { path: /api, upstreams: [127.0.0.1:3001] }
",
    );
}

#[test]
fn same_path_on_different_listens_is_allowed() {
    build_server(
        "
version: 1
clusters:
  - name: a
    listen: 127.0.0.1:1
    routes: [{ path: /api, upstreams: [127.0.0.1:3000] }]
  - name: b
    listen: 127.0.0.1:2
    routes: [{ path: /api, upstreams: [127.0.0.1:3001] }]
",
    );
}

#[test]
#[should_panic(expected = "bad upstream")]
fn invalid_upstream_address_panics() {
    build_server(
        "
version: 1
clusters:
  - name: a
    listen: 127.0.0.1:1
    routes: [{ path: /, upstreams: [not-an-address] }]
",
    );
}

#[test]
#[should_panic(expected = "tls/http2 differ")]
fn clusters_sharing_listen_must_agree_on_protocol() {
    build_server(
        "
version: 1
clusters:
  - name: a
    listen: 127.0.0.1:1
    routes: [{ path: /a, upstreams: [127.0.0.1:3000] }]
  - name: b
    listen: 127.0.0.1:1
    http2: true
    routes: [{ path: /b, upstreams: [127.0.0.1:3001] }]
",
    );
}

#[test]
#[should_panic(expected = "bad tls cert/key")]
fn missing_tls_files_panic() {
    build_server(
        "
version: 1
clusters:
  - name: a
    listen: 127.0.0.1:1
    tls: { cert: /nonexistent/server.crt, key: /nonexistent/server.key }
    routes: [{ path: /, upstreams: [127.0.0.1:3000] }]
",
    );
}

#[test]
#[should_panic(expected = "invalid config")]
fn invalid_yaml_panics() {
    build_server("version: 1\nclusters: not-a-list");
}

// ---------------------------------------------------------------------------
// end-to-end: real proxy + mock upstreams
// ---------------------------------------------------------------------------

#[test]
fn e2e_routes_by_longest_path_prefix() {
    let api = mock_upstream("api", true);
    let api_v2 = mock_upstream("api-v2", true);
    let one = mock_upstream("one", true);
    let site = mock_upstream("site", true);
    let only = mock_upstream("only", true);
    let (shared, single) = (free_port(), free_port());

    start_proxy(
        format!(
            "
version: 1
threads: 1
clusters:
  - name: main
    listen: 127.0.0.1:{shared}
    routes:
      - {{ path: /api, upstreams: [{api}] }}
      - {{ path: /api/v2, upstreams: [{api_v2}] }}
      - {{ path: /one, upstreams: [{one}] }}
  # second cluster on the same listen is merged into the same listener
  - name: site
    listen: 127.0.0.1:{shared}
    routes:
      - {{ path: /, upstreams: [{site}] }}
  # separate listener without a catch-all route
  - name: only
    listen: 127.0.0.1:{single}
    routes:
      - {{ path: /only, upstreams: [{only}] }}
"
        ),
        &[shared, single],
    );

    for (path, expected) in [
        ("/api", "api"),
        ("/api/users", "api"),
        ("/api/v2", "api-v2"),
        ("/api/v2/users", "api-v2"),
        ("/one", "one"),
        ("/one/two", "one"),
        ("/apis", "site"), // not under /api -> falls through to /
        ("/", "site"),
        ("/anything", "site"),
    ] {
        assert_eq!(get(shared, path), (200, expected.into()), "GET {path}");
    }

    assert_eq!(get(single, "/only/x"), (200, "only".into()));
    assert_eq!(get(single, "/").0, 404);
    assert_eq!(get(single, "/api").0, 404);
}

#[test]
fn e2e_round_robin_uses_every_upstream() {
    let (a, b) = (mock_upstream("a", true), mock_upstream("b", true));
    let port = free_port();
    start_proxy(
        format!(
            "
version: 1
threads: 1
clusters:
  - name: rr
    listen: {port}
    routes:
      - {{ path: /, algorithm: round_robin, upstreams: [{a}, {b}] }}
"
        ),
        &[port],
    );

    let bodies: Vec<String> = (0..4).map(|_| get(port, "/").1).collect();
    assert_eq!(bodies.iter().collect::<HashSet<_>>().len(), 2, "{bodies:?}");
    assert_ne!(
        bodies[0], bodies[1],
        "round robin should alternate: {bodies:?}"
    );
}

#[test]
fn e2e_hash_and_consistent_keep_client_on_one_upstream() {
    let (a, b, c) = (
        mock_upstream("a", true),
        mock_upstream("b", true),
        mock_upstream("c", true),
    );
    let port = free_port();
    start_proxy(
        format!(
            "
version: 1
threads: 1
clusters:
  - name: sticky
    listen: 127.0.0.1:{port}
    routes:
      - {{ path: /hash, algorithm: hash, upstreams: [{a}, {b}, {c}] }}
      - {{ path: /consistent, algorithm: consistent, upstreams: [{a}, {b}, {c}] }}
"
        ),
        &[port],
    );

    for path in ["/hash", "/consistent"] {
        let bodies: HashSet<String> = (0..10).map(|_| get(port, path).1).collect();
        assert_eq!(bodies.len(), 1, "{path} moved the client: {bodies:?}");
    }
}

#[test]
fn e2e_rate_limit_returns_429_per_route() {
    let up = mock_upstream("up", true);
    let port = free_port();
    start_proxy(
        format!(
            "
version: 1
threads: 1
clusters:
  - name: limited
    listen: 127.0.0.1:{port}
    routes:
      - path: /limited
        rate_limit: {{ requests: 3, window_secs: 60 }}
        upstreams: [{up}]
      - path: /free
        upstreams: [{up}]
"
        ),
        &[port],
    );

    let statuses: Vec<u16> = (0..5).map(|_| get(port, "/limited").0).collect();
    assert_eq!(statuses, [200, 200, 200, 429, 429]);

    // other routes are not affected by /limited's limit
    for _ in 0..10 {
        assert_eq!(get(port, "/free").0, 200);
    }
}

#[test]
fn e2e_rate_limit_resets_after_window() {
    let up = mock_upstream("up", true);
    let port = free_port();
    start_proxy(
        format!(
            "
version: 1
threads: 1
clusters:
  - name: limited
    listen: 127.0.0.1:{port}
    routes:
      - path: /
        rate_limit: {{ requests: 2, window_secs: 1 }}
        upstreams: [{up}]
"
        ),
        &[port],
    );

    let first: Vec<u16> = (0..3).map(|_| get(port, "/").0).collect();
    assert_eq!(first, [200, 200, 429]);

    thread::sleep(Duration::from_millis(1100));
    assert_eq!(get(port, "/").0, 200, "limit should reset in a new window");
}

#[test]
fn e2e_health_checks_remove_bad_upstreams() {
    let alive = mock_upstream("alive", true);
    let sick = mock_upstream("sick", false); // up, but /health returns 500
    let (dead, dead2) = (dead_upstream(), dead_upstream());
    let port = free_port();
    start_proxy(
        format!(
            "
version: 1
threads: 1
clusters:
  - name: hc
    listen: 127.0.0.1:{port}
    routes:
      - path: /tcp
        health_check: {{ type: tcp, interval_secs: 1 }}
        upstreams: [{alive}, {dead}]
      - path: /http
        health_check: {{ type: http, path: /health, interval_secs: 1 }}
        upstreams: [{alive}, {sick}]
      - path: /all-down
        health_check: {{ type: tcp, interval_secs: 1 }}
        upstreams: [{dead2}]
"
        ),
        &[port],
    );

    // give the health checks time to run
    thread::sleep(Duration::from_secs(3));

    for path in ["/tcp", "/http"] {
        for _ in 0..6 {
            assert_eq!(get(port, path), (200, "alive".into()), "GET {path}");
        }
    }

    // no healthy upstream left -> the proxy cannot pick a peer
    assert_eq!(get(port, "/all-down").0, 500);
}

/// Opens an HTTP/2 cleartext connection (prior knowledge) and returns the type of
/// the first frame the server sends back. An HTTP/2 server must start with SETTINGS (0x4).
fn h2c_first_frame_type(port: u16) -> Option<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .unwrap();
    // empty SETTINGS frame: length 0, type 0x4, flags 0, stream 0
    stream.write_all(&[0, 0, 0, 0x4, 0, 0, 0, 0, 0]).unwrap();
    let mut header = [0u8; 9];
    stream.read_exact(&mut header).ok()?;
    Some(header[3])
}

#[test]
fn e2e_http2_cleartext_when_enabled_and_http1_still_works() {
    let upstream = mock_upstream("up", true);
    let (h1_port, h2_port) = (free_port(), free_port());
    start_proxy(
        format!(
            "
version: 1
threads: 1
clusters:
  - name: h1
    listen: {h1_port}
    routes: [{{ path: /, upstreams: [{upstream}] }}]
  - name: h2
    listen: {h2_port}
    http2: true
    routes: [{{ path: /, upstreams: [{upstream}] }}]
"
        ),
        &[h1_port, h2_port],
    );

    assert_eq!(
        h2c_first_frame_type(h2_port),
        Some(0x4),
        "expected SETTINGS"
    );
    assert_ne!(
        h2c_first_frame_type(h1_port),
        Some(0x4),
        "HTTP/2 must be off by default"
    );
    // HTTP/1.1 clients are still served on the HTTP/2-enabled listener
    assert_eq!(get(h2_port, "/"), (200, "up".into()));
    assert_eq!(get(h1_port, "/"), (200, "up".into()));
}
