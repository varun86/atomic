//! Shared infrastructure for atomic-server end-to-end tests.
//!
//! The same `Backend` switch used in atomic-core's pipeline tests runs each
//! suite against SQLite (always) and Postgres (when `ATOMIC_TEST_DATABASE_URL`
//! is set). Each `TestCtx` owns an `AppState` backed by the chosen store plus
//! a freshly minted API token; `test_app(&ctx)` produces an actix-web `App`
//! that mirrors the real server's `/api` scope (auth wrapper + full route
//! registration).
//!
//! The wiremock-backed `MockAiServer` is duplicated from
//! `atomic-core/tests/support/mod.rs` rather than shared via a feature flag —
//! making it a dual-purpose lib feature would pull `wiremock` into production
//! atomic-core builds. The protocol surface (OpenAI-compat embeddings +
//! chat/completions) is stable enough that one-time duplication is cheaper
//! than the feature-flag plumbing.

#![allow(dead_code)] // Helpers are per-test; not every test uses every helper.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use actix_web::{web, App};
use atomic_core::DatabaseManager;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::broadcast;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use atomic_server::auth::BearerAuth;
use atomic_server::export_jobs::ExportJobManager;
use atomic_server::log_buffer::LogBuffer;
use atomic_server::mcp::AtomicMcpTransport;
use atomic_server::mcp_auth::McpAuth;
use atomic_server::routes;
use atomic_server::state::{AppState, SetupClaimLimiter};

/// Embedding dimension used by the mock. Matches the default
/// `openai_compat_embedding_dimension` so no dimension reconciliation kicks
/// in mid-test.
pub const EMBED_DIM: usize = 1536;

// ==================== Backend switch ====================

pub enum Backend {
    Sqlite,
    Postgres,
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Sqlite => "sqlite",
            Backend::Postgres => "postgres",
        }
    }
}

/// Test context — owns the temp dir (so the SQLite DB stays alive), the
/// AppState, and the raw bearer token. Drop order matters: `_temp` lives
/// strictly longer than `state` because handlers may still be flushing.
pub struct TestCtx {
    _temp: Option<TempDir>,
    pub state: web::Data<AppState>,
    pub token: String,
    pub mock: Arc<MockAiServer>,
}

impl TestCtx {
    /// Build a fresh test context on the chosen backend. Returns `None` when
    /// the Postgres URL is unset so individual tests can skip cleanly rather
    /// than failing on a missing env var.
    pub async fn new(backend: Backend) -> Option<Self> {
        let mock = Arc::new(MockAiServer::start().await);

        let (manager, temp) = match backend {
            Backend::Sqlite => {
                let dir = TempDir::new().expect("create tempdir");
                let manager = Arc::new(
                    DatabaseManager::new(dir.path()).expect("open sqlite manager"),
                );
                (manager, Some(dir))
            }
            Backend::Postgres => {
                let url = std::env::var("ATOMIC_TEST_DATABASE_URL").ok()?;
                truncate_postgres(&url).await;
                let dir = TempDir::new().expect("create tempdir");
                let manager = Arc::new(
                    DatabaseManager::new_postgres(dir.path(), &url)
                        .await
                        .expect("open postgres manager"),
                );
                // Tempdir holds the export_jobs work tree; the manager itself
                // ignores it for Postgres backends.
                (manager, Some(dir))
            }
        };

        // Configure the active core to use the mock AI provider so the
        // embedding + tagging pipeline runs end-to-end during tests.
        let core = manager.active_core().await.expect("active core");
        for (k, v) in [
            ("provider", "openai_compat"),
            ("openai_compat_base_url", mock.base_url().as_str()),
            ("openai_compat_api_key", "test-key"),
            ("openai_compat_embedding_model", "mock-embed"),
            ("openai_compat_llm_model", "mock-llm"),
            ("openai_compat_embedding_dimension", "1536"),
            ("auto_tagging_enabled", "true"),
        ] {
            core.set_setting(k, v).await.expect("seed test setting");
        }
        core.configure_autotag_targets(&["Topics".to_string()], &[])
            .await
            .expect("configure autotag targets");

        let (_info, raw_token) = core
            .create_api_token("e2e-test")
            .await
            .expect("mint api token");

        let temp_for_exports = temp
            .as_ref()
            .map(|d| d.path().to_path_buf())
            .unwrap_or_else(|| std::env::temp_dir().join("atomic-e2e-exports"));
        let (event_tx, _) = broadcast::channel(64);
        let state = web::Data::new(AppState {
            manager,
            event_tx,
            public_url: None,
            log_buffer: LogBuffer::new(16),
            export_jobs: ExportJobManager::for_tests(temp_for_exports.join("exports")),
            setup_token: None,
            dangerously_skip_setup_token: true,
            setup_claim_lock: tokio::sync::Mutex::new(()),
            setup_claim_limiter: SetupClaimLimiter::new(),
        });

        Some(TestCtx {
            _temp: temp,
            state,
            token: raw_token,
            mock,
        })
    }

    pub fn auth_header(&self) -> (&'static str, String) {
        ("Authorization", format!("Bearer {}", self.token))
    }

    pub fn db_header(&self, db_id: &str) -> (&'static str, String) {
        ("X-Atomic-Database", db_id.to_string())
    }
}

/// Build an actix-web `App` that mirrors the real server's `/api` scope:
/// `BearerAuth` middleware around the full set of authenticated routes.
/// Public routes (`/health`, OAuth discovery, setup) are intentionally
/// omitted — they have their own coverage in `api_atoms.rs` and don't
/// participate in the per-backend e2e contract.
pub fn test_app(
    ctx: &TestCtx,
) -> App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new().app_data(ctx.state.clone()).service(
        web::scope("/api")
            .wrap(BearerAuth {
                state: ctx.state.clone(),
            })
            .configure(routes::configure_routes),
    )
}

// ==================== Mock AI server ====================

pub struct MockAiServer {
    server: MockServer,
    counters: Arc<MockAiCounters>,
}

#[derive(Default)]
struct MockAiCounters {
    embedding_requests: AtomicUsize,
    chat_requests: AtomicUsize,
}

impl MockAiServer {
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        let counters = Arc::new(MockAiCounters::default());

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(EmbedResponder {
                counters: counters.clone(),
            })
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ChatResponder {
                counters: counters.clone(),
            })
            .mount(&server)
            .await;

        Self { server, counters }
    }

    pub fn base_url(&self) -> String {
        self.server.uri()
    }

    pub fn embedding_request_count(&self) -> usize {
        self.counters.embedding_requests.load(Ordering::Relaxed)
    }

    pub fn chat_request_count(&self) -> usize {
        self.counters.chat_requests.load(Ordering::Relaxed)
    }
}

/// Bag-of-words style unit-vector embedder. Same construction as the
/// atomic-core test support; identical vector layout keeps cross-suite
/// thresholds comparable.
fn embed_text(text: &str) -> Vec<f32> {
    let mut vec = vec![0.0f32; EMBED_DIM];
    for word in text.split_whitespace() {
        let normalized: String = word
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if normalized.is_empty() {
            continue;
        }
        let mut h = DefaultHasher::new();
        normalized.hash(&mut h);
        let idx = (h.finish() as usize) % EMBED_DIM;
        vec[idx] += 1.0;
    }
    let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in vec.iter_mut() {
            *v /= norm;
        }
    } else {
        vec[0] = 1.0;
    }
    vec
}

struct EmbedResponder {
    counters: Arc<MockAiCounters>,
}

impl Respond for EmbedResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.counters
            .embedding_requests
            .fetch_add(1, Ordering::Relaxed);
        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return ResponseTemplate::new(400),
        };
        let Some(inputs) = body.get("input").and_then(|v| v.as_array()) else {
            return ResponseTemplate::new(400);
        };
        let data: Vec<Value> = inputs
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let text = text.as_str().unwrap_or_default();
                json!({
                    "object": "embedding",
                    "index": index,
                    "embedding": embed_text(text),
                })
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": data,
            "model": body.get("model").cloned().unwrap_or(Value::Null),
        }))
    }
}

struct ChatResponder {
    counters: Arc<MockAiCounters>,
}

impl Respond for ChatResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.counters.chat_requests.fetch_add(1, Ordering::Relaxed);
        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return ResponseTemplate::new(400),
        };

        let schema_name = body
            .pointer("/response_format/json_schema/name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let request_text = body.to_string().to_lowercase();

        let content = match schema_name {
            "extraction_result" => {
                let tag_name = if request_text.contains("biology") {
                    "Biology"
                } else if request_text.contains("cooking") || request_text.contains("pasta") {
                    "Cooking"
                } else {
                    "Physics"
                };
                json!({
                    "tags": [
                        { "name": tag_name, "parent_name": "Topics" },
                    ]
                })
                .to_string()
            }
            _ => "{}".to_string(),
        };

        ResponseTemplate::new(200).set_body_json(json!({
            "id": "mock-cmpl",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": content,
                    },
                    "finish_reason": "stop",
                }
            ],
        }))
    }
}

// ==================== Postgres truncate ====================

/// Truncate per-DB tables on the shared Postgres test instance. Matches the
/// list used in atomic-core/tests/storage_tests.rs and pipeline support.
pub async fn truncate_postgres(url: &str) {
    use sqlx::postgres::PgPoolOptions;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .expect("connect truncate pool");
    let _ = sqlx::raw_sql(
        "TRUNCATE atoms, tags, atom_tags, atom_chunks, atom_positions, atom_pipeline_jobs, \
         semantic_edges, atom_clusters, tag_embeddings, \
         wiki_articles, wiki_citations, wiki_links, wiki_article_versions, atom_links, \
         conversations, conversation_tags, chat_messages, chat_tool_calls, chat_citations, \
         feeds, feed_tags, feed_items, settings, \
         briefing_citations, briefings, oauth_codes, oauth_clients, api_tokens \
         CASCADE",
    )
    .execute(&pool)
    .await;
}

// ==================== Real-port test server ====================

/// Handle to a `HttpServer` running on an ephemeral port. Drop the handle (or
/// call [`stop`](LiveServer::stop)) to shut it down.
pub struct LiveServer {
    pub base_url: String,
    handle: actix_web::dev::ServerHandle,
}

impl LiveServer {
    pub async fn stop(self) {
        self.handle.stop(false).await;
    }
}

/// Start a real `HttpServer` on `127.0.0.1:0` mirroring the production route
/// table (`/api` scope behind `BearerAuth`, plus the public `/ws` endpoint).
/// The returned `base_url` points at the bound port; the server runs on its
/// own tokio task until the handle is stopped.
///
/// Used by the WebSocket and concurrent-storm suites that need a real TCP
/// listener — `actix_web::test::init_service` is in-process only and can't
/// satisfy `actix-ws`'s upgrade response or model real concurrent HTTP load.
pub async fn spawn_live_server(ctx: &TestCtx) -> LiveServer {
    use actix_web::{web, App, HttpServer};
    use std::time::Duration;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let base_url = format!("http://{}", addr);

    // MCP transport must be constructed once and cloned into each worker so
    // the LocalSessionManager is shared. Mirrors the wiring in main.rs.
    let mcp_transport = AtomicMcpTransport::new(
        std::sync::Arc::clone(&ctx.state.manager),
        ctx.state.event_tx.clone(),
        Duration::from_secs(30),
    );

    let state_for_factory = ctx.state.clone();
    let server = HttpServer::new(move || {
        let state = state_for_factory.clone();
        App::new()
            .app_data(state.clone())
            .route("/ws", web::get().to(atomic_server::ws::ws_handler))
            .service(
                web::scope("/mcp")
                    .wrap(McpAuth {
                        state: state.clone(),
                    })
                    .service(mcp_transport.clone().scope()),
            )
            .service(
                web::scope("/api")
                    .wrap(BearerAuth {
                        state: state.clone(),
                    })
                    .configure(routes::configure_routes),
            )
    })
    .workers(1)
    .listen(listener)
    .expect("attach listener")
    .run();

    let handle = server.handle();
    actix_web::rt::spawn(server);

    LiveServer { base_url, handle }
}

// ==================== Pipeline poller ====================

/// Poll `GET /api/atoms/{id}` until `embedding_status` reaches a terminal
/// state (`complete` or `failed`). Returns the parsed atom body. The mock
/// embedder responds instantly, but the pipeline runs on a background tokio
/// task — without polling, tests would race the response.
pub async fn poll_until_embedding_done<S, B>(
    app: &S,
    auth: (&'static str, String),
    atom_id: &str,
) -> Value
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    use actix_web::test as actix_test;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let req = actix_test::TestRequest::get()
            .uri(&format!("/api/atoms/{}", atom_id))
            .insert_header(auth.clone())
            .to_request();
        let resp = actix_test::call_service(app, req).await;
        assert_eq!(resp.status(), 200, "atom should exist while polling");
        let body: Value = actix_test::read_body_json(resp).await;
        let status = body["embedding_status"].as_str().unwrap_or("");
        if status == "complete" || status == "failed" {
            return body;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "embedding did not reach terminal state for {atom_id} within 15s; \
                 last status = {status:?}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
