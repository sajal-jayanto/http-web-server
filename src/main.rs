use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora::lb::health_check::{HttpHealthCheck, TcpHealthCheck};
use pingora::lb::selection::{
    BackendIter, BackendSelection, Consistent, FNVHash, Random, RoundRobin,
};
use pingora::lb::{Backend, LoadBalancer};
use pingora::prelude::*;
use pingora::proxy::http_proxy_service_with_name;
use pingora::server::configuration::{Opt, ServerConf};
use pingora_limits::rate::Rate;
use serde::Deserialize;

const CONFIG_PATH: &str = "config/web-conf.yaml";
const DEFAULT_PID_FILE: &str = "/tmp/http-web-server.pid";
const DEFAULT_UPGRADE_SOCK: &str = "/tmp/http-web-server_upgrade.sock";

#[derive(Deserialize)]
struct Config {
    clusters: Vec<ClusterConfig>,
}

#[derive(Deserialize)]
struct ClusterConfig {
    name: String,
    #[serde(deserialize_with = "listen_addr")]
    listen: String,
    routes: Vec<RouteConfig>,
}

const DEFAULT_HOST: &str = "0.0.0.0";

fn listen_addr<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Listen {
        Port(u16),
        Addr(String),
    }
    Ok(match Listen::deserialize(d)? {
        Listen::Port(port) => format!("{DEFAULT_HOST}:{port}"),
        Listen::Addr(addr) if !addr.contains(':') => format!("{DEFAULT_HOST}:{addr}"),
        Listen::Addr(addr) => addr,
    })
}

#[derive(Deserialize)]
struct RouteConfig {
    #[serde(default = "default_path")]
    path: String,
    #[serde(default)]
    algorithm: Algorithm,
    upstreams: Vec<String>,
    rate_limit: Option<RateLimitConfig>,
    health_check: Option<HealthCheckConfig>,
}

#[derive(Deserialize)]
struct HealthCheckConfig {
    #[serde(default, rename = "type")]
    kind: HealthCheckKind,
    #[serde(default = "default_path")]
    path: String,
    #[serde(default = "default_interval")]
    interval_secs: u64,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum HealthCheckKind {
    #[default]
    Tcp,
    Http,
}

fn default_path() -> String {
    "/".into()
}

fn default_interval() -> u64 {
    5
}

#[derive(Deserialize)]
struct RateLimitConfig {
    requests: isize,
    #[serde(default = "default_window")]
    window_secs: u64,
}

fn default_window() -> u64 {
    1
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum Algorithm {
    #[default]
    RoundRobin,
    Random,
    Hash,
    Consistent,
}

enum Balancer {
    RoundRobin(Arc<LoadBalancer<RoundRobin>>),
    Random(Arc<LoadBalancer<Random>>),
    Hash(Arc<LoadBalancer<FNVHash>>),
    Consistent(Arc<LoadBalancer<Consistent>>),
}

impl Balancer {
    fn new(name: &str, route: &RouteConfig, server: &mut Server) -> std::io::Result<Self> {
        Ok(match route.algorithm {
            Algorithm::RoundRobin => Self::RoundRobin(build_lb(name, route, server)?),
            Algorithm::Random => Self::Random(build_lb(name, route, server)?),
            Algorithm::Hash => Self::Hash(build_lb(name, route, server)?),
            Algorithm::Consistent => Self::Consistent(build_lb(name, route, server)?),
        })
    }

    fn select(&self, key: &[u8]) -> Option<Backend> {
        match self {
            Self::RoundRobin(lb) => lb.select(key, 256),
            Self::Random(lb) => lb.select(key, 256),
            Self::Hash(lb) => lb.select(key, 256),
            Self::Consistent(lb) => lb.select(key, 256),
        }
    }
}

fn build_lb<S>(
    name: &str,
    route: &RouteConfig,
    server: &mut Server,
) -> std::io::Result<Arc<LoadBalancer<S>>>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    let mut lb = LoadBalancer::<S>::try_from_iter(&route.upstreams)?;
    let Some(hc) = &route.health_check else {
        return Ok(Arc::new(lb));
    };

    match hc.kind {
        HealthCheckKind::Tcp => lb.set_health_check(TcpHealthCheck::new()),
        HealthCheckKind::Http => {
            let mut check = HttpHealthCheck::new("localhost", false);
            check.req = RequestHeader::build("GET", hc.path.as_bytes(), None).unwrap();
            check.req.append_header("Host", "localhost").unwrap();
            lb.set_health_check(Box::new(check));
        }
    }
    lb.health_check_frequency = Some(Duration::from_secs(hc.interval_secs));

    let service = background_service(&format!("health check {name}"), lb);
    let lb = service.task();
    server.add_service(service);
    Ok(lb)
}

struct RateLimiter {
    rate: Rate,
    max_requests: isize,
}

struct Route {
    path: String,
    balancer: Balancer,
    rate_limiter: Option<RateLimiter>,
}

fn path_matches(prefix: &str, path: &str) -> bool {
    match path.strip_prefix(prefix) {
        Some(rest) => prefix.ends_with('/') || rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

struct Router {
    routes: Vec<Route>, 
}

#[derive(Default)]
struct Ctx {
    client_ip: String,
    route: usize,
}

#[async_trait]
impl ProxyHttp for Router {
    type CTX = Ctx;

    fn new_ctx(&self) -> Ctx {
        Ctx::default()
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Ctx) -> Result<bool> {
        let path = session.req_header().uri.path();
        let route = self.routes.iter().position(|r| path_matches(&r.path, path));
        let Some(index) = route else {
            session.respond_error(404).await?;
            return Ok(true);
        };
        ctx.route = index;
        ctx.client_ip = session
            .client_addr()
            .and_then(|addr| addr.as_inet())
            .map(|addr| addr.ip().to_string())
            .unwrap_or_default();

        if let Some(limiter) = &self.routes[index].rate_limiter {
            if limiter.rate.observe(&ctx.client_ip, 1) > limiter.max_requests {
                session.respond_error(429).await?;
                return Ok(true); // response sent, stop here
            }
        }
        Ok(false)
    }

    async fn upstream_peer(&self, _session: &mut Session, ctx: &mut Ctx) -> Result<Box<HttpPeer>> {
        let upstream = self.routes[ctx.route]
            .balancer
            .select(ctx.client_ip.as_bytes())
            .ok_or_else(|| Error::new_str("no upstream"))?;
        Ok(Box::new(HttpPeer::new(upstream, false, String::new())))
    }
}

#[cfg(test)]
mod tests;

fn main() {
    let path = std::env::var("CONFIG").unwrap_or_else(|_| CONFIG_PATH.into());
    let raw = std::fs::read_to_string(&path).expect("failed to read config");
    build_server(&raw).run_forever();
}

fn server_conf(raw: &str) -> ServerConf {
    let mut yaml: serde_yaml::Mapping = serde_yaml::from_str(raw).expect("invalid config");
    for (key, default) in [
        ("pid_file", DEFAULT_PID_FILE),
        ("upgrade_sock", DEFAULT_UPGRADE_SOCK),
    ] {
        yaml.entry(key.into()).or_insert(default.into());
    }
    ServerConf::from_yaml(&serde_yaml::to_string(&yaml).unwrap()).unwrap()
}

fn build_server(raw: &str) -> Server {
    let config: Config = serde_yaml::from_str(raw).expect("invalid config");
    let mut server = Server::new_with_opt_and_conf(Opt::default(), server_conf(raw));
    server.bootstrap();

    let mut routers: BTreeMap<String, Router> = BTreeMap::new();
    for cluster in config.clusters {
        let router = routers
            .entry(cluster.listen)
            .or_insert(Router { routes: vec![] });

        for route in cluster.routes {
            let name = format!("{} {}", cluster.name, route.path);
            if router.routes.iter().any(|r| r.path == route.path) {
                panic!("{name}: path already used on this listen");
            }
            let balancer = Balancer::new(&name, &route, &mut server)
                .unwrap_or_else(|e| panic!("{name}: bad upstream: {e}"));
            let rate_limiter = route.rate_limit.map(|limit| RateLimiter {
                rate: Rate::new(Duration::from_secs(limit.window_secs)),
                max_requests: limit.requests,
            });
            router.routes.push(Route {
                path: route.path,
                balancer,
                rate_limiter,
            });
        }
    }

    for (listen, mut router) in routers {
        router
            .routes
            .sort_by(|a, b| b.path.len().cmp(&a.path.len()));
        let mut service = http_proxy_service_with_name(&server.configuration, router, &listen);
        service.add_tcp(&listen);
        server.add_service(service);
    }

    server
}
