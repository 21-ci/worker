use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, Request, Response, StatusCode, Uri},
    routing::{get, post},
    Router,
};
use dashmap::DashMap;
use http_body_util::BodyExt;
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore};
use wasmtime::{
    component::{Component, Linker, ResourceTable},
    Config, Engine, InstanceAllocationStrategy, OptLevel, PoolingAllocationConfig, ResourceLimiter,
    Store,
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    p2::{
        bindings::{http::types::Scheme, Proxy, ProxyPre},
        body::HyperIncomingBody,
        WasiHttpCtxView, WasiHttpView,
    },
    WasiHttpCtx,
};

struct AppState {
    engine: Engine,
    modules: DashMap<String, ProxyPre<HostState>>,
    pools: DashMap<String, Arc<ComponentPool>>,
    // host -> list of (base_path, func_id). base_path is "" for root mounts
    // or "/segment[/segment...]" otherwise. Lookup picks the longest base
    // that segment-prefix-matches the request path.
    domains: DashMap<String, Vec<(String, String)>>,
    wasm_files_dir: std::path::PathBuf,
    inherit_logs: bool,
    print_stats: bool,
    auth_token: Option<String>,
    inflight: Arc<Semaphore>,
    // None = unlimited (env var was negative/unset). Used only for the 503
    // message; when unlimited, the semaphore is sized so try_acquire never
    // fails in practice.
    inflight_max: Option<usize>,
    prewarm_target: usize,
    refill_workers: usize,
}

// One ready-to-use, freshly-instantiated component instance. Consumed once per
// request, then dropped. The pool's refill task replaces it in the background.
struct Warm {
    store: Store<HostState>,
    proxy: Proxy,
    peak: Arc<AtomicUsize>,
}

struct ComponentPool {
    pre: ProxyPre<HostState>,
    engine: Engine,
    inherit_logs: bool,
    target: usize,
    queue: StdMutex<VecDeque<Warm>>,
    notify: Notify,
}

struct PeakMemory(Arc<AtomicUsize>);

impl ResourceLimiter for PeakMemory {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let prev = self.0.load(Ordering::Relaxed);
        if desired > prev {
            self.0.store(desired, Ordering::Relaxed);
        }
        Ok(true)
    }
    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

struct HostState {
    wasi_ctx: WasiCtx,
    http_ctx: WasiHttpCtx,
    table: ResourceTable,
    limiter: PeakMemory,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http_ctx,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

#[tokio::main]
async fn main() {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.cranelift_opt_level(OptLevel::Speed);

    let pool_size: u32 = std::env::var("POOL_INSTANCES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8192);
    if pool_size > 0 {
        let mut pool = PoolingAllocationConfig::default();
        pool.total_component_instances(pool_size);
        pool.total_core_instances(pool_size);
        pool.total_memories(pool_size);
        pool.total_tables(pool_size);
        config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
        println!("🧱 Pooling allocator: {pool_size} instances");
    } else {
        config.allocation_strategy(InstanceAllocationStrategy::OnDemand);
        println!("🧱 OnDemand allocator (POOL_INSTANCES=0)");
    }

    let engine = Engine::new(&config).expect("Failed to create Wasmtime Engine");

    let wasm_files_dir = std::env::var("WASM_FILES_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("wasm_files")
        });
    let inherit_logs = env_flag("WASM_LOGS");
    let print_stats = env_flag("STATS_LOG");
    let auth_token = std::env::var("AUTH_TOKEN").ok().filter(|s| !s.is_empty());
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string());
    let domain_bind_addr =
        std::env::var("DOMAIN_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3030".to_string());
    let prewarm_target: usize = std::env::var("PREWARM_TARGET")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    // INFLIGHT_MAX: negative (default) = unlimited. Non-negative = hard cap;
    // requests beyond it get a 503 with a DDoS-flavored message.
    let inflight_max: Option<usize> = match std::env::var("INFLIGHT_MAX")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
    {
        Some(n) if n >= 0 => Some(n as usize),
        _ => None,
    };
    // REFILL_WORKERS: parallel pre-warm refill tasks per component. Default
    // ~half of available cores. Helps keep the pool non-empty under bursty
    // load (does not raise sustained throughput — that's CPU-bound).
    let refill_workers: usize = std::env::var("REFILL_WORKERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| (n.get() / 2).max(1))
                .unwrap_or(1)
        });

    let domains = DashMap::new();
    load_domains(&wasm_files_dir, &domains).await;
    let domain_count: usize = domains.iter().map(|e| e.value().len()).sum();

    let state = Arc::new(AppState {
        engine,
        modules: DashMap::new(),
        pools: DashMap::new(),
        domains,
        wasm_files_dir: wasm_files_dir.clone(),
        inherit_logs,
        print_stats,
        auth_token: auth_token.clone(),
        inflight: Arc::new(Semaphore::new(
            inflight_max.unwrap_or(Semaphore::MAX_PERMITS),
        )),
        inflight_max,
        prewarm_target,
        refill_workers,
    });

    // Eagerly preload every component already on disk so .cwasm cache is used
    // and pools start filling before the first request lands. Runs in the
    // background, so binding the listener still happens immediately.
    tokio::spawn(preload_all(state.clone()));

    let app = Router::new()
        .route("/init", post(init_wasm))
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .route("/:func_id", get(handle_root).post(handle_root))
        .route("/:func_id/*path", get(handle_path).post(handle_path))
        .with_state(state.clone());

    let domain_app = Router::new()
        .fallback(domain_dispatch)
        .with_state(state.clone());

    println!("🚀 worker on http://{bind_addr}");
    println!("🌐 domain router on http://{domain_bind_addr} ({domain_count} mapping(s))");
    println!("📂 lazy-loading from {}", wasm_files_dir.display());
    println!(
        "⚙️  WASM_LOGS={} STATS_LOG={} AUTH={} PREWARM={} REFILLERS={} INFLIGHT_MAX={}",
        inherit_logs,
        print_stats,
        if auth_token.is_some() { "on" } else { "OFF (no AUTH_TOKEN set)" },
        prewarm_target,
        refill_workers,
        match inflight_max {
            Some(n) => n.to_string(),
            None => "unlimited".to_string(),
        },
    );

    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
    let dlistener = tokio::net::TcpListener::bind(&domain_bind_addr).await.unwrap();

    let main_srv = axum::serve(listener, app);
    let dom_srv = axum::serve(dlistener, domain_app);
    let (_, _) = tokio::join!(main_srv, dom_srv);
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("on")
    )
}

fn build_proxy_pre(engine: &Engine, component: &Component) -> anyhow::Result<ProxyPre<HostState>> {
    let mut linker: Linker<HostState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    Ok(ProxyPre::new(linker.instantiate_pre(component)?)?)
}

// Compile from raw .wasm bytes and write a .cwasm sidecar next to it. Returns
// the compiled `Component`. Subsequent loads of the same func_id should go
// through `load_component_cached` instead, which skips cranelift entirely.
async fn compile_and_cache(
    engine: &Engine,
    bytes: &[u8],
    cwasm_path: &std::path::Path,
) -> anyhow::Result<Component> {
    let component = Component::new(engine, bytes)?;
    let serialized = component.serialize()?;
    if let Some(dir) = cwasm_path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = cwasm_path.with_extension("cwasm.tmp");
    tokio::fs::write(&tmp, &serialized).await?;
    tokio::fs::rename(&tmp, cwasm_path).await?;
    Ok(component)
}

// Load a component for `func_id`. Prefer the precompiled `.cwasm` sidecar
// (mmap + relocate, ~10s of ms even for big wasm). Fall back to `.wasm`
// (cranelift compile, seconds-to-minutes) and write the `.cwasm` for next
// time.
async fn load_component_cached(
    engine: &Engine,
    wasm_files_dir: &std::path::Path,
    func_id: &str,
) -> anyhow::Result<Component> {
    let cwasm_path = wasm_files_dir.join(format!("{func_id}.cwasm"));
    if cwasm_path.exists() {
        // SAFETY: we produced this file ourselves with this exact wasmtime
        // version + Engine config. Worker never accepts .cwasm uploads.
        let component = unsafe { Component::deserialize_file(engine, &cwasm_path)? };
        return Ok(component);
    }
    let wasm_path = wasm_files_dir.join(format!("{func_id}.wasm"));
    let bytes = tokio::fs::read(&wasm_path).await?;
    compile_and_cache(engine, &bytes, &cwasm_path).await
}

#[derive(Deserialize)]
struct InitParams {
    name: Option<String>,
    domain: Option<String>,
}

async fn init_wasm(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<InitParams>,
    body: axum::body::Bytes,
) -> (StatusCode, String) {
    if let Some(expected) = &state.auth_token {
        let presented = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")));
        if presented != Some(expected.as_str()) {
            return (
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token".to_string(),
            );
        }
    }

    let func_id = match params.name {
        Some(n) => {
            if !valid_name(&n) {
                return (
                    StatusCode::BAD_REQUEST,
                    "invalid name: 1-128 chars, [a-zA-Z0-9_-] only, not 'init'".to_string(),
                );
            }
            n
        }
        None => uuid::Uuid::new_v4().to_string(),
    };

    let wasm_path = state.wasm_files_dir.join(format!("{func_id}.wasm"));
    if state.modules.contains_key(&func_id) || wasm_path.exists() {
        return (
            StatusCode::CONFLICT,
            format!("'{func_id}' already exists"),
        );
    }

    let domain_mount = match params.domain.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(d) => match parse_domain(d) {
            Ok(parsed) => {
                let (host, base) = &parsed;
                let already = state
                    .domains
                    .get(host)
                    .map(|e| e.value().iter().any(|(b, _)| b == base))
                    .unwrap_or(false);
                if already {
                    return (
                        StatusCode::CONFLICT,
                        format!("domain '{host}{base}' already registered"),
                    );
                }
                Some(parsed)
            }
            Err(e) => return (StatusCode::BAD_REQUEST, format!("invalid domain: {e}")),
        },
        None => None,
    };

    let cwasm_path = state.wasm_files_dir.join(format!("{func_id}.cwasm"));
    let component = match compile_and_cache(&state.engine, &body, &cwasm_path).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("Invalid WASM Component: {e}")),
    };
    let pre = match build_proxy_pre(&state.engine, &component) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("link: {e}")),
    };

    if let Err(e) = persist_wasm(&wasm_path, &body).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write {}: {e}", wasm_path.display()),
        );
    }

    if let Some((host, base)) = &domain_mount {
        let sidecar = state.wasm_files_dir.join(format!("{func_id}.domain"));
        let serialized = format!("{host}{base}");
        if let Err(e) = tokio::fs::write(&sidecar, serialized.as_bytes()).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("write {}: {e}", sidecar.display()),
            );
        }
        state
            .domains
            .entry(host.clone())
            .or_default()
            .push((base.clone(), func_id.clone()));
    }

    state.modules.insert(func_id.clone(), pre.clone());
    spawn_pool(&state, &func_id, pre);
    (StatusCode::CREATED, func_id)
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "init"
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// Parse "host" or "host/base/path" into (host_lower, base_normalized).
// base_normalized is "" for root mounts, else "/seg[/seg...]" (no trailing
// slash). Host: 1-253 chars of [a-zA-Z0-9.-]. Base segments: [a-zA-Z0-9._-].
fn parse_domain(raw: &str) -> Result<(String, String), String> {
    let raw = raw.trim().trim_start_matches('/');
    let (host_part, base_part) = match raw.split_once('/') {
        Some((h, b)) => (h, b),
        None => (raw, ""),
    };
    let host = host_part.to_ascii_lowercase();
    if host.is_empty() || host.len() > 253 {
        return Err("host must be 1-253 chars".into());
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err("host: only [a-zA-Z0-9.-] allowed".into());
    }
    let base_trimmed = base_part.trim_matches('/');
    let base = if base_trimmed.is_empty() {
        String::new()
    } else {
        for seg in base_trimmed.split('/') {
            if seg.is_empty()
                || !seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
            {
                return Err("base path: segments must be [a-zA-Z0-9._-]".into());
            }
        }
        format!("/{base_trimmed}")
    };
    Ok((host, base))
}

async fn load_domains(dir: &std::path::Path, domains: &DashMap<String, Vec<(String, String)>>) {
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(r) => r,
        Err(_) => return,
    };
    while let Ok(Some(ent)) = rd.next_entry().await {
        let p = ent.path();
        if p.extension().and_then(|s| s.to_str()) != Some("domain") {
            continue;
        }
        let func_id = match p.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let raw = match tokio::fs::read_to_string(&p).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let Ok((host, base)) = parse_domain(raw.trim()) else {
            continue;
        };
        domains.entry(host).or_default().push((base, func_id));
    }
}

// segment-aware prefix: "/a" matches "/a" and "/a/b" but not "/ab".
fn base_matches(base: &str, path: &str) -> bool {
    if base.is_empty() {
        return true;
    }
    if !path.starts_with(base) {
        return false;
    }
    let rest = &path[base.len()..];
    rest.is_empty() || rest.starts_with('/')
}

async fn domain_dispatch(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response<Body> {
    // Fast-path: nothing enrolled with a domain mount. Skip Host parsing,
    // ascii-lowercasing, longest-prefix scan — straight to 404.
    if state.domains.is_empty() {
        return err(StatusCode::NOT_FOUND, "no domain mappings registered");
    }

    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(':').next().unwrap_or(s).to_ascii_lowercase());

    let host = match host {
        Some(h) if !h.is_empty() => h,
        _ => return err(StatusCode::NOT_FOUND, "missing Host header"),
    };

    let path = req.uri().path().to_string();

    let mount = state.domains.get(&host).and_then(|entry| {
        entry
            .value()
            .iter()
            .filter(|(base, _)| base_matches(base, &path))
            .max_by_key(|(base, _)| base.len())
            .map(|(base, func_id)| (base.clone(), func_id.clone()))
    });

    let (base, func_id) = match mount {
        Some(m) => m,
        None => return err(StatusCode::NOT_FOUND, "no domain mapping for host/path"),
    };

    let inner_path = path[base.len()..]
        .trim_start_matches('/')
        .to_string();
    handle(state, func_id, inner_path, req).await
}

async fn persist_wasm(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = path.with_extension("wasm.tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

async fn handle_root(
    State(state): State<Arc<AppState>>,
    Path(func_id): Path<String>,
    req: Request<Body>,
) -> Response<Body> {
    handle(state, func_id, String::new(), req).await
}

async fn handle_path(
    State(state): State<Arc<AppState>>,
    Path((func_id, path)): Path<(String, String)>,
    req: Request<Body>,
) -> Response<Body> {
    handle(state, func_id, path, req).await
}

async fn handle(
    state: Arc<AppState>,
    func_id: String,
    inner_path: String,
    mut req: Request<Body>,
) -> Response<Body> {
    let t_total = Instant::now();

    // Hard concurrency cap. Refuse load beyond INFLIGHT_MAX before any work.
    let _permit = match state.inflight.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            let cap = state
                .inflight_max
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unlimited".to_string());
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!(
                    "⚠️  rate limit exceeded ({cap} concurrent requests in flight) — \
                     possible DDoS attack"
                ),
            );
        }
    };

    // Lazy-load + pool spawn on first hit. Eager preload usually does this
    // earlier; this branch covers components added out-of-band or first-ever
    // requests racing the startup scan.
    if !state.pools.contains_key(&func_id) {
        let wasm_path = state.wasm_files_dir.join(format!("{func_id}.wasm"));
        if !wasm_path.exists() {
            return finalize(
                err(StatusCode::NOT_FOUND, &format!("Component not found at {}", wasm_path.display())),
                &state, &func_id, 0, 0, t_total,
            );
        }
        match load_component_cached(&state.engine, &state.wasm_files_dir, &func_id).await {
            Ok(component) => match build_proxy_pre(&state.engine, &component) {
                Ok(pre) => {
                    state.modules.insert(func_id.clone(), pre.clone());
                    spawn_pool(&state, &func_id, pre);
                    println!("✅ Loaded: {func_id}");
                }
                Err(e) => {
                    return finalize(
                        err(StatusCode::BAD_REQUEST, &format!("link: {e}")),
                        &state, &func_id, 0, 0, t_total,
                    );
                }
            },
            Err(e) => {
                return finalize(
                    err(StatusCode::BAD_REQUEST, &format!("load: {e}")),
                    &state, &func_id, 0, 0, t_total,
                );
            }
        }
    }

    let pool = state.pools.get(&func_id).unwrap().clone();

    // Pre-instantiated instance off the queue, or instantiate on demand if
    // the pool was drained.
    let mut warm = match pool.take() {
        Some(w) => w,
        None => match build_warm(&pool).await {
            Ok(w) => w,
            Err(e) => {
                return finalize(
                    err(StatusCode::INTERNAL_SERVER_ERROR, &format!("instantiate: {e}")),
                    &state, &func_id, 0, 0, t_total,
                );
            }
        },
    };

    let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
    let new_uri_str = format!("/{inner_path}{query}");
    if let Ok(uri) = new_uri_str.parse::<Uri>() {
        *req.uri_mut() = uri;
    }

    // Stream the request body straight into the guest instead of buffering
    // it into `Bytes` first. Saves an allocate+copy per request (irrelevant
    // for hello-world, big win for non-trivial payloads), removes the
    // 256 MiB body-size DoS surface, and starts driving the guest as soon
    // as the first chunk arrives.
    let (parts, body) = req.into_parts();
    let hyper_body: HyperIncomingBody = body
        .map_err(|e| {
            wasmtime_wasi_http::p2::bindings::http::types::ErrorCode::InternalError(Some(
                e.to_string(),
            ))
        })
        .boxed_unsync();
    let hyper_req = Request::from_parts(parts, hyper_body);

    warm.peak.store(0, Ordering::Relaxed);
    let peak = warm.peak.clone();

    let incoming = match warm
        .store
        .data_mut()
        .http()
        .new_incoming_request(Scheme::Http, hyper_req)
    {
        Ok(r) => r,
        Err(e) => {
            return finalize(
                err(StatusCode::INTERNAL_SERVER_ERROR, &format!("incoming: {e}")),
                &state, &func_id, 0, peak.load(Ordering::Relaxed), t_total,
            );
        }
    };
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let outparam = match warm.store.data_mut().http().new_response_outparam(sender) {
        Ok(o) => o,
        Err(e) => {
            return finalize(
                err(StatusCode::INTERNAL_SERVER_ERROR, &format!("outparam: {e}")),
                &state, &func_id, 0, peak.load(Ordering::Relaxed), t_total,
            );
        }
    };

    let t_invoke = Instant::now();
    let Warm { store, proxy, .. } = warm;
    let task = tokio::spawn(async move {
        proxy
            .wasi_http_incoming_handler()
            .call_handle(store, incoming, outparam)
            .await
    });

    let received = receiver.await;
    let invoke_us = t_invoke.elapsed().as_micros() as u64;

    // We consumed one warm instance; wake the refiller to top the pool back
    // up in the background.
    pool.notify.notify_one();

    let resp = match received {
        Ok(Ok(resp)) => {
            let (parts, body) = resp.into_parts();
            let body = Body::new(body.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                Box::new(std::io::Error::other(e.to_string()))
            }));
            Response::from_parts(parts, body)
        }
        Ok(Err(code)) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("WASM error: {code:?}"),
        ),
        Err(_) => match task.await {
            Ok(Ok(())) => err(StatusCode::INTERNAL_SERVER_ERROR, "no response sent"),
            Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("trap: {e}")),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("join: {e}")),
        },
    };

    let mem = peak.load(Ordering::Relaxed);
    finalize(resp, &state, &func_id, invoke_us, mem, t_total)
}

fn spawn_pool(state: &Arc<AppState>, func_id: &str, pre: ProxyPre<HostState>) {
    let pool = Arc::new(ComponentPool {
        pre,
        engine: state.engine.clone(),
        inherit_logs: state.inherit_logs,
        target: state.prewarm_target,
        queue: StdMutex::new(VecDeque::with_capacity(state.prewarm_target)),
        notify: Notify::new(),
    });
    state.pools.insert(func_id.to_string(), pool.clone());
    if state.prewarm_target > 0 {
        for _ in 0..state.refill_workers.max(1) {
            tokio::spawn(refill_loop(pool.clone()));
        }
    }
}

impl ComponentPool {
    fn take(&self) -> Option<Warm> {
        self.queue.lock().unwrap().pop_front()
    }
}

async fn build_warm(pool: &ComponentPool) -> wasmtime::Result<Warm> {
    let mut wasi_builder = WasiCtxBuilder::new();
    if pool.inherit_logs {
        wasi_builder.inherit_stdout().inherit_stderr();
    }
    let peak = Arc::new(AtomicUsize::new(0));
    let host = HostState {
        wasi_ctx: wasi_builder.build(),
        http_ctx: WasiHttpCtx::new(),
        table: ResourceTable::new(),
        limiter: PeakMemory(peak.clone()),
    };
    let mut store = Store::new(&pool.engine, host);
    store.limiter(|s| &mut s.limiter as &mut dyn ResourceLimiter);
    let proxy = pool.pre.instantiate_async(&mut store).await?;
    Ok(Warm { store, proxy, peak })
}

async fn refill_loop(pool: Arc<ComponentPool>) {
    loop {
        let need = {
            let q = pool.queue.lock().unwrap();
            pool.target.saturating_sub(q.len())
        };
        if need == 0 {
            pool.notify.notified().await;
            continue;
        }
        match build_warm(&pool).await {
            Ok(w) => {
                pool.queue.lock().unwrap().push_back(w);
            }
            Err(_) => {
                // Backoff on instantiation failure (usually pooling allocator
                // saturation). Let things drain.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

// Walk wasm_files_dir, load every component once (using .cwasm cache when
// present), and spawn its pool so pre-warming starts before any request.
// Per-component work runs in parallel via spawn so a slow .wasm doesn't block
// the rest.
async fn preload_all(state: Arc<AppState>) {
    let mut rd = match tokio::fs::read_dir(&state.wasm_files_dir).await {
        Ok(r) => r,
        Err(_) => return,
    };
    while let Ok(Some(ent)) = rd.next_entry().await {
        let p = ent.path();
        if p.extension().and_then(|s| s.to_str()) != Some("wasm") {
            continue;
        }
        let Some(func_id) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        if state.pools.contains_key(&func_id) {
            continue;
        }
        let st = state.clone();
        tokio::spawn(async move {
            let t = Instant::now();
            match load_component_cached(&st.engine, &st.wasm_files_dir, &func_id).await {
                Ok(component) => match build_proxy_pre(&st.engine, &component) {
                    Ok(pre) => {
                        st.modules.insert(func_id.clone(), pre.clone());
                        spawn_pool(&st, &func_id, pre);
                        println!("✅ Preloaded {func_id} in {:?}", t.elapsed());
                    }
                    Err(e) => eprintln!("⚠️  link {func_id}: {e}"),
                },
                Err(e) => eprintln!("⚠️  load {func_id}: {e}"),
            }
        });
    }
}

fn finalize(
    resp: Response<Body>,
    state: &AppState,
    func_id: &str,
    invoke_us: u64,
    mem_bytes: usize,
    t_total: Instant,
) -> Response<Body> {
    let total_us = t_total.elapsed().as_micros() as u64;

//  Unused for now, but will be helpful when writing a wrapper for 21 cloud
//     let h = resp.headers_mut();
//     h.insert(
//         "x-wasm-peak-mem-bytes",
//         HeaderValue::from_str(&mem_bytes.to_string()).unwrap(),
//     );
//     h.insert(
//         "x-wasm-invoke-us",
//         HeaderValue::from_str(&invoke_us.to_string()).unwrap(),
//     );
//     h.insert(
//         "x-wasm-total-us",
//         HeaderValue::from_str(&total_us.to_string()).unwrap(),
//     );

    if state.print_stats {
        eprintln!(
            "[stats] func={func_id} status={} mem={mem_bytes}B invoke={invoke_us}us total={total_us}us",
            resp.status().as_u16()
        );
    }
    resp
}

fn err(status: StatusCode, msg: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from(msg.to_string()))
        .unwrap()
}
