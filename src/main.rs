use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, Request, Response, StatusCode, Uri},
    routing::{get, post},
    Router,
};
use dashmap::DashMap;
use http_body_util::{BodyExt, Full};
use serde::Deserialize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use wasmtime::{
    component::{Component, Linker, ResourceTable},
    Config, Engine, InstanceAllocationStrategy, OptLevel, PoolingAllocationConfig, ResourceLimiter,
    Store,
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    p2::{
        bindings::{http::types::Scheme, ProxyPre},
        body::HyperIncomingBody,
        WasiHttpCtxView, WasiHttpView,
    },
    WasiHttpCtx,
};

struct AppState {
    engine: Engine,
    modules: DashMap<String, ProxyPre<HostState>>,
    wasm_files_dir: std::path::PathBuf,
    inherit_logs: bool,
    print_stats: bool,
    auth_token: Option<String>,
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

    let state = Arc::new(AppState {
        engine,
        modules: DashMap::new(),
        wasm_files_dir: wasm_files_dir.clone(),
        inherit_logs,
        print_stats,
        auth_token: auth_token.clone(),
    });

    let app = Router::new()
        .route("/init", post(init_wasm))
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .route("/:func_id", get(handle_root).post(handle_root))
        .route("/:func_id/*path", get(handle_path).post(handle_path))
        .with_state(state);

    println!("🚀 worker on http://{bind_addr}");
    println!("📂 lazy-loading from {}", wasm_files_dir.display());
    println!(
        "⚙️  WASM_LOGS={} STATS_LOG={} AUTH={}",
        inherit_logs,
        print_stats,
        if auth_token.is_some() { "on" } else { "OFF (no AUTH_TOKEN set)" }
    );

    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("on")
    )
}

fn create_proxy_pre(engine: &Engine, bytes: &[u8]) -> anyhow::Result<ProxyPre<HostState>> {
    let component = Component::new(engine, bytes)?;
    let mut linker: Linker<HostState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    Ok(ProxyPre::new(linker.instantiate_pre(&component)?)?)
}

#[derive(Deserialize)]
struct InitParams {
    name: Option<String>,
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

    let pre = match create_proxy_pre(&state.engine, &body) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("Invalid WASM Component: {e}")),
    };

    if let Err(e) = persist_wasm(&wasm_path, &body).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write {}: {e}", wasm_path.display()),
        );
    }

    state.modules.insert(func_id.clone(), pre);
    (StatusCode::CREATED, func_id)
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "init"
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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

    if !state.modules.contains_key(&func_id) {
        let wasm_path = state.wasm_files_dir.join(format!("{func_id}.wasm"));
        if !wasm_path.exists() {
            return finalize(
                err(StatusCode::NOT_FOUND, &format!("Component not found at {}", wasm_path.display())),
                &state, &func_id, 0, 0, t_total,
            );
        }
        let bytes = match tokio::fs::read(&wasm_path).await {
            Ok(b) => b,
            Err(e) => {
                return finalize(
                    err(StatusCode::INTERNAL_SERVER_ERROR, &format!("Disk: {e}")),
                    &state, &func_id, 0, 0, t_total,
                );
            }
        };
        match create_proxy_pre(&state.engine, &bytes) {
            Ok(pre) => {
                state.modules.insert(func_id.clone(), pre);
                println!("✅ Warmed up: {func_id}");
            }
            Err(e) => {
                return finalize(
                    err(StatusCode::BAD_REQUEST, &format!("Compile: {e}")),
                    &state, &func_id, 0, 0, t_total,
                );
            }
        }
    }

    let pre = state.modules.get(&func_id).unwrap().clone();

    let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
    let new_uri_str = format!("/{inner_path}{query}");
    if let Ok(uri) = new_uri_str.parse::<Uri>() {
        *req.uri_mut() = uri;
    }

    let (parts, body) = req.into_parts();
    let bytes = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            return finalize(
                err(StatusCode::BAD_REQUEST, &format!("body: {e}")),
                &state, &func_id, 0, 0, t_total,
            );
        }
    };
    let hyper_body: HyperIncomingBody = Full::new(bytes)
        .map_err(|e: std::convert::Infallible| match e {})
        .boxed_unsync();
    let hyper_req = Request::from_parts(parts, hyper_body);

    let mut wasi_builder = WasiCtxBuilder::new();
    if state.inherit_logs {
        wasi_builder.inherit_stdout().inherit_stderr();
    }
    let peak = Arc::new(AtomicUsize::new(0));
    let host = HostState {
        wasi_ctx: wasi_builder.build(),
        http_ctx: WasiHttpCtx::new(),
        table: ResourceTable::new(),
        limiter: PeakMemory(peak.clone()),
    };
    let mut store = Store::new(&state.engine, host);
    store.limiter(|s| &mut s.limiter as &mut dyn ResourceLimiter);

    let incoming = match store
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
    let outparam = match store.data_mut().http().new_response_outparam(sender) {
        Ok(o) => o,
        Err(e) => {
            return finalize(
                err(StatusCode::INTERNAL_SERVER_ERROR, &format!("outparam: {e}")),
                &state, &func_id, 0, peak.load(Ordering::Relaxed), t_total,
            );
        }
    };

    let t_invoke = Instant::now();
    let task = tokio::spawn(async move {
        let proxy = pre.instantiate_async(&mut store).await?;
        proxy
            .wasi_http_incoming_handler()
            .call_handle(store, incoming, outparam)
            .await
    });

    let received = receiver.await;
    let invoke_us = t_invoke.elapsed().as_micros() as u64;

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
