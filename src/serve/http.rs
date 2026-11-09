//! The dashboard's HTTP side (ARCHITECTURE §14): sign-in, the embedded page, the JSON API, the
//! live board stream and the terminal socket. Every handler does its work on the blocking pool
//! through `view.rs`; nothing here knows what a thread is.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, watch};

use super::{term, view};
use crate::config::ProjectConfig;
use crate::pr::Gh;
use crate::routines::{self, Draft, Patch, Trigger};
use crate::tools::{self, ToolCtx};
use crate::{db, paths};

const COOKIE: &str = "chief_token";
/// How often a watched project's board is read again, as the terminal board does (§10).
const BOARD_EVERY: Duration = Duration::from_secs(2);
const PR_EVERY: Duration = Duration::from_secs(30);
/// How long a project's refresher outlives its last viewer (a page reload comes back sooner).
const HUB_LINGER: Duration = Duration::from_secs(10);

const INDEX: &str = include_str!("web/index.html");
const LOCKED: &str = include_str!("web/locked.html");
const ASSETS: [(&str, &str, &str); 6] = [
    (
        "app.js",
        "text/javascript; charset=utf-8",
        include_str!("web/app.js"),
    ),
    (
        "app.css",
        "text/css; charset=utf-8",
        include_str!("web/app.css"),
    ),
    ("icon.svg", "image/svg+xml", include_str!("web/icon.svg")),
    (
        "xterm.js",
        "text/javascript; charset=utf-8",
        include_str!("web/vendor/xterm.js"),
    ),
    (
        "xterm.css",
        "text/css; charset=utf-8",
        include_str!("web/vendor/xterm.css"),
    ),
    (
        "addon-fit.js",
        "text/javascript; charset=utf-8",
        include_str!("web/vendor/addon-fit.js"),
    ),
];

/// One project's live board: the latest snapshot as JSON text, refreshed while anyone watches.
struct Hub {
    board: watch::Sender<Arc<str>>,
    /// An action just ran: read again now rather than at the next tick.
    kick: Notify,
}

pub struct App {
    token: String,
    hubs: Mutex<HashMap<String, Arc<Hub>>>,
}

type Shared = Arc<App>;

/// An API failure: the status and a message for the page to show.
pub struct ApiError(StatusCode, String);

/// The project exists but isn't shown: no such project, archived, or its dashboard is off.
#[derive(Debug)]
struct NotServed(String);

impl std::fmt::Display for NotServed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotServed {}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let status = if e.downcast_ref::<NotServed>().is_some() {
            StatusCode::NOT_FOUND
        } else {
            // What the tools refuse is the caller's to fix, and said in plain words.
            StatusCode::BAD_REQUEST
        };
        ApiError(status, format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

type Api = Result<Json<Value>, ApiError>;

/// Runs `f` with `slug`'s tool context, when the dashboard may show that project.
pub fn in_project<T>(slug: &str, f: impl FnOnce(&ToolCtx) -> Result<T>) -> Result<T> {
    let valid = !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    let dir = paths::project_dir(slug);
    if !valid || !dir.join("project.toml").exists() {
        return Err(NotServed(format!("no project {slug:?}")).into());
    }
    tools::with_project(slug, |ctx| {
        if !ctx.config.dashboard.enabled {
            return Err(NotServed(format!(
                "the dashboard of {slug} is off; `chief dashboard {slug}` turns it on"
            ))
            .into());
        }
        if db::archived_at(ctx.conn, slug)?.is_some() {
            return Err(NotServed(format!(
                "{slug} is archived; `chief unarchive {slug}` brings it back"
            ))
            .into());
        }
        f(ctx)
    })
}

/// `f` on the blocking pool: the tools shell out and wait.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result.map_err(ApiError::from),
        Err(e) => Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the request crashed: {e}"),
        )),
    }
}

/// Same length or not, every byte is compared: the time taken says nothing about the token.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// How a request proved it has the token.
#[derive(Debug, PartialEq)]
enum Proof {
    /// The browser's cookie, which it sends by itself: such a request must also come from this
    /// page (`same_origin`).
    Cookie,
    /// An `Authorization: Bearer` header, which only a client that knows the token writes.
    Bearer,
}

fn proof(token: &str, headers: &HeaderMap) -> Option<Proof> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if bearer.is_some_and(|b| same(b.trim(), token)) {
        return Some(Proof::Bearer);
    }
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies
        .split(';')
        .filter_map(|c| c.trim().split_once('='))
        .any(|(name, value)| name == COOKIE && same(value, token))
        .then_some(Proof::Cookie)
}

/// Whether the request was made by a page this server served: its `Origin` names the host it
/// was sent to. A page elsewhere can make the browser send a request here, but not under this
/// origin.
fn same_origin(headers: &HeaderMap) -> bool {
    let text = |name| {
        headers
            .get(name)
            .and_then(|v: &HeaderValue| v.to_str().ok())
    };
    let (Some(origin), Some(host)) = (text(header::ORIGIN), text(header::HOST)) else {
        return false;
    };
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .is_some_and(|o| o.eq_ignore_ascii_case(host))
}

/// Everything but the sign-in and the static files needs the token; anything that changes
/// state, and the terminal socket, also needs to come from this page when the proof is the
/// cookie.
async fn guard(State(app): State<Shared>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let open = path == "/login" || path.starts_with("/assets/");
    if !open {
        let api = path.starts_with("/api/");
        let Some(proof) = proof(&app.token, request.headers()) else {
            return if api {
                ApiError(
                    StatusCode::UNAUTHORIZED,
                    "not signed in: open the link `chief dashboard <project>` prints".to_string(),
                )
                .into_response()
            } else {
                (StatusCode::UNAUTHORIZED, page(LOCKED)).into_response()
            };
        };
        let reads = matches!(*request.method(), Method::GET | Method::HEAD);
        let socket = request.headers().contains_key(header::UPGRADE);
        if proof == Proof::Cookie && (!reads || socket) && !same_origin(request.headers()) {
            return ApiError(
                StatusCode::FORBIDDEN,
                "this request didn't come from the dashboard's own page".to_string(),
            )
            .into_response();
        }
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             connect-src 'self' ws: wss:; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    response
}

fn page(html: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response()
}

#[derive(Deserialize)]
struct Login {
    token: Option<String>,
    next: Option<String>,
}

/// `GET /login?token=…&next=/p/<slug>`: trades the token in the link for a cookie and goes on,
/// so the token leaves the address bar.
async fn login(State(app): State<Shared>, Query(q): Query<Login>) -> Response {
    if !q.token.as_deref().is_some_and(|t| same(t, &app.token)) {
        return (StatusCode::UNAUTHORIZED, page(LOCKED)).into_response();
    }
    // Only a path on this server: never a redirect somewhere else.
    let next = q
        .next
        .filter(|n| n.starts_with('/') && !n.starts_with("//") && !n.contains('\\'))
        .unwrap_or_else(|| "/".to_string());
    let cookie = format!(
        "{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000",
        app.token
    );
    let mut response = Redirect::to(&next).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

async fn index() -> Response {
    page(INDEX)
}

async fn asset(Path(name): Path<String>) -> Response {
    match ASSETS.iter().find(|(n, _, _)| *n == name) {
        Some((_, kind, body)) => (
            [
                (header::CONTENT_TYPE, *kind),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            *body,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The projects whose dashboard is on, with what needs the user in each.
fn projects() -> Result<Value> {
    let conn = db::open(&paths::db())?;
    let mut out = Vec::new();
    for (slug, dir) in db::list_projects(&conn)? {
        if db::archived_at(&conn, &slug)?.is_some() {
            continue;
        }
        let Ok(config) = ProjectConfig::load(&std::path::Path::new(&dir).join("project.toml"))
        else {
            continue;
        };
        if !config.dashboard.enabled {
            continue;
        }
        let threads = db::list_sessions(&conn, &slug, Some(crate::state::Role::Thread))?;
        let count = |group: &str| {
            threads
                .iter()
                .filter(|t| !t.resolved && t.group_name == group)
                .count()
        };
        out.push(json!({
            "slug": slug,
            "name": config.name,
            "goal": config.goal,
            "waiting": count("waiting"),
            "working": count("working"),
            "open": threads.iter().filter(|t| !t.resolved).count(),
            "routines": routines::list(&conn, &slug)?.iter().filter(|r| r.enabled).count(),
        }));
    }
    Ok(json!({"projects": out, "version": env!("CARGO_PKG_VERSION")}))
}

async fn api_projects() -> Api {
    Ok(Json(blocking(projects).await?))
}

async fn api_board(Path(slug): Path<String>) -> Api {
    let board = blocking(move || in_project(&slug, |ctx| view::board(ctx, false))).await?;
    Ok(Json(board))
}

async fn api_session(Path((slug, name)): Path<(String, String)>) -> Api {
    let detail = blocking(move || in_project(&slug, |ctx| view::detail(ctx, &name))).await?;
    Ok(Json(detail))
}

async fn api_act(
    State(app): State<Shared>,
    Path((slug, name, action)): Path<(String, String, String)>,
) -> Api {
    let project = slug.clone();
    let done = blocking(move || in_project(&slug, |ctx| view::act(ctx, &name, &action))).await;
    app.kick(&project);
    Ok(Json(done?))
}

async fn api_create_routine(
    State(app): State<Shared>,
    Path(slug): Path<String>,
    Json(draft): Json<Draft>,
) -> Api {
    let project = slug.clone();
    let made = blocking(move || {
        in_project(&slug, |ctx| {
            let made = routines::create(ctx.conn, ctx.config, &draft, (ctx.now)())?;
            routines::ensure_folder(ctx.project_dir, &made)?;
            routines::view(ctx.conn, &made)
        })
    })
    .await?;
    app.kick(&project);
    Ok(Json(made))
}

async fn api_update_routine(
    State(app): State<Shared>,
    Path((slug, name)): Path<(String, String)>,
    Json(changes): Json<Patch>,
) -> Api {
    let project = slug.clone();
    let saved = blocking(move || {
        in_project(&slug, |ctx| {
            routines::update(ctx.conn, ctx.config, &name, &changes, (ctx.now)())
                .and_then(|r| routines::view(ctx.conn, &r))
        })
    })
    .await?;
    app.kick(&project);
    Ok(Json(saved))
}

async fn api_delete_routine(
    State(app): State<Shared>,
    Path((slug, name)): Path<(String, String)>,
) -> Api {
    let project = slug.clone();
    blocking(move || {
        in_project(&slug, |ctx| {
            routines::delete(ctx.conn, &ctx.config.slug, &name)
        })
    })
    .await?;
    app.kick(&project);
    Ok(Json(json!({"ok": true})))
}

async fn api_run_routine(
    State(app): State<Shared>,
    Path((slug, name)): Path<(String, String)>,
) -> Api {
    let project = slug.clone();
    let ran = blocking(move || {
        in_project(&slug, |ctx| {
            // The rows as they are now decide whether its last run still works.
            let _ = tools::refresh(ctx);
            routines::run(ctx, &name, Trigger::Manual).map(|o| o.to_json(&name))
        })
    })
    .await;
    app.kick(&project);
    Ok(Json(ran?))
}

/// One routine in full, for its page: all its notes, its findings and its series.
async fn api_routine(Path((slug, name)): Path<(String, String)>) -> Api {
    let routine =
        blocking(move || in_project(&slug, |ctx| routines::read(ctx, &name, 256 * 1024, 40, 400)))
            .await?;
    Ok(Json(routine))
}

/// The project's standing brief and memory index, to read.
async fn api_files(Path(slug): Path<String>) -> Api {
    let files = blocking(move || {
        in_project(&slug, |ctx| {
            let read = |rel: &str| std::fs::read_to_string(ctx.project_dir.join(rel)).ok();
            Ok(json!({
                "instructions": read("instructions.md"),
                "memory": read("memory/MEMORY.md"),
            }))
        })
    })
    .await?;
    Ok(Json(files))
}

impl App {
    /// Makes the project's refresher read now, if anyone watches it.
    fn kick(&self, slug: &str) {
        if let Some(hub) = self.hubs.lock().ok().and_then(|h| h.get(slug).cloned()) {
            hub.kick.notify_one();
        }
    }

    /// A receiver of `slug`'s board, starting its refresher when this is the first viewer.
    fn watch(self: &Arc<Self>, slug: &str) -> watch::Receiver<Arc<str>> {
        let mut hubs = self.hubs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hub) = hubs.get(slug) {
            return hub.board.subscribe();
        }
        let (board, receiver) = watch::channel(Arc::<str>::from(""));
        let hub = Arc::new(Hub {
            board,
            kick: Notify::new(),
        });
        hubs.insert(slug.to_string(), hub.clone());
        tokio::spawn(refresher(self.clone(), slug.to_string(), hub));
        receiver
    }
}

/// Keeps one project's board fresh while someone watches: every `BOARD_EVERY` it syncs with
/// `claude agents` and publishes the board when it changed; every `PR_EVERY` it refreshes pull
/// requests (and cleans up merged threads when the project says so, §12), as the terminal board
/// does. It ends `HUB_LINGER` after its last viewer left.
async fn refresher(app: Shared, slug: String, hub: Arc<Hub>) {
    let mut alone_since: Option<Instant> = None;
    let mut pr_at: Option<Instant> = None;
    loop {
        if hub.board.receiver_count() == 0 {
            let since = *alone_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= HUB_LINGER {
                // Looked at again under the lock `watch` subscribes under, so a viewer arriving
                // now either finds the hub still there and keeps it alive, or makes a new one.
                let mut hubs = app.hubs.lock().unwrap_or_else(|e| e.into_inner());
                if hub.board.receiver_count() == 0 {
                    hubs.remove(&slug);
                    return;
                }
            }
        } else {
            alone_since = None;
        }
        if pr_at.is_none_or(|at| at.elapsed() >= PR_EVERY) {
            pr_at = Some(Instant::now());
            let project = slug.clone();
            let _ = tokio::task::spawn_blocking(move || {
                in_project(&project, |ctx| {
                    let gh = Gh::from_env();
                    let cleanup = crate::cleanup::Ctx {
                        conn: ctx.conn,
                        claude: ctx.claude,
                        gh: &gh,
                        now: ctx.now,
                    };
                    crate::pr::refresh_project(&cleanup, ctx.config).map(|_| ())
                })
            })
            .await;
        }
        let project = slug.clone();
        let read =
            tokio::task::spawn_blocking(move || in_project(&project, |ctx| view::board(ctx, true)))
                .await;
        let text = match read {
            Ok(Ok(board)) => board.to_string(),
            Ok(Err(e)) => json!({"gone": format!("{e:#}")}).to_string(),
            Err(e) => json!({"error": format!("reading the board crashed: {e}")}).to_string(),
        };
        hub.board.send_if_modified(|current| {
            if **current == *text {
                return false;
            }
            *current = Arc::from(text.as_str());
            true
        });
        tokio::select! {
            _ = tokio::time::sleep(BOARD_EVERY) => {}
            _ = hub.kick.notified() => {}
        }
    }
}

/// `GET /api/p/<slug>/stream`: the board as server-sent events, one `board` event per change.
async fn api_stream(State(app): State<Shared>, Path(slug): Path<String>) -> Response {
    let project = slug.clone();
    if let Err(e) = blocking(move || in_project(&project, |_| Ok(()))).await {
        return e.into_response();
    }
    let receiver = app.watch(&slug);
    // The value a new viewer finds is one it hasn't seen: send it first, then every change.
    let stream =
        futures_util::stream::unfold((receiver, true), |(mut receiver, first)| async move {
            loop {
                if !first || receiver.borrow().is_empty() {
                    receiver.changed().await.ok()?;
                }
                let board = receiver.borrow_and_update().clone();
                if !board.is_empty() {
                    let event = Event::default().event("board").data(&*board);
                    return Some((Ok::<_, Infallible>(event), (receiver, false)));
                }
            }
        });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
        .into_response()
}

#[derive(Deserialize)]
struct TermQuery {
    /// A thread's name; none is the coordinator.
    target: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
}

/// `GET /api/p/<slug>/term`: a WebSocket carrying `claude attach` for one session.
async fn api_term(
    Path(slug): Path<String>,
    Query(q): Query<TermQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let project = slug.clone();
    if let Err(e) = blocking(move || in_project(&project, |_| Ok(()))).await {
        return e.into_response();
    }
    let size = (
        q.cols.unwrap_or(100).clamp(20, 500),
        q.rows.unwrap_or(30).clamp(5, 300),
    );
    upgrade.on_upgrade(move |socket| term::run(socket, slug, q.target, size))
}

pub fn router(token: String) -> Router {
    let app = Arc::new(App {
        token,
        hubs: Mutex::new(HashMap::new()),
    });
    Router::new()
        .route("/", get(index))
        .route("/p/{slug}", get(index))
        .route("/login", get(login))
        .route("/assets/{name}", get(asset))
        .route("/api/projects", get(api_projects))
        .route("/api/p/{slug}/board", get(api_board))
        .route("/api/p/{slug}/stream", get(api_stream))
        .route("/api/p/{slug}/files", get(api_files))
        .route("/api/p/{slug}/term", get(api_term))
        .route("/api/p/{slug}/sessions/{name}", get(api_session))
        .route("/api/p/{slug}/threads/{name}/{action}", post(api_act))
        .route("/api/p/{slug}/routines", post(api_create_routine))
        .route(
            "/api/p/{slug}/routines/{name}",
            get(api_routine)
                .patch(api_update_routine)
                .delete(api_delete_routine),
        )
        .route("/api/p/{slug}/routines/{name}/run", post(api_run_routine))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

/// Serves until SIGINT or SIGTERM.
pub async fn serve(listener: tokio::net::TcpListener, token: String) -> Result<()> {
    let stop = async {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    };
    // Open streams and terminals would hold a graceful shutdown forever: stop accepting, and
    // let the caller drop the runtime.
    tokio::select! {
        served = axum::serve(listener, router(token)) => served.context("serving the dashboard"),
        _ = stop => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn only_the_exact_token_signs_in() {
        let token = "s3cret-token-of-enough-length";
        assert_eq!(
            proof(
                token,
                &headers(&[("cookie", "a=b; chief_token=s3cret-token-of-enough-length")])
            ),
            Some(Proof::Cookie)
        );
        assert_eq!(
            proof(
                token,
                &headers(&[("authorization", "Bearer s3cret-token-of-enough-length")])
            ),
            Some(Proof::Bearer)
        );
        for bad in [
            vec![],
            vec![("cookie", "chief_token=s3cret-token-of-enough-lengt")],
            vec![("cookie", "chief_token=s3cret-token-of-enough-lengthX")],
            vec![("cookie", "other=s3cret-token-of-enough-length")],
            vec![("cookie", "chief_token=")],
            vec![("authorization", "Basic s3cret-token-of-enough-length")],
            vec![("authorization", "Bearer nope")],
        ] {
            assert_eq!(proof(token, &headers(&bad)), None, "{bad:?}");
        }
        assert!(same("", ""));
        assert!(!same("a", ""));
    }

    #[test]
    fn a_request_from_another_page_is_not_same_origin() {
        let ok = [
            [
                ("origin", "http://127.0.0.1:4747"),
                ("host", "127.0.0.1:4747"),
            ],
            [
                ("origin", "http://Localhost:4747"),
                ("host", "localhost:4747"),
            ],
            [
                ("origin", "https://box.tailnet.ts.net"),
                ("host", "box.tailnet.ts.net"),
            ],
        ];
        for pairs in ok {
            assert!(same_origin(&headers(&pairs)), "{pairs:?}");
        }
        let bad: [&[(&str, &str)]; 5] = [
            &[
                ("origin", "http://evil.example"),
                ("host", "127.0.0.1:4747"),
            ],
            &[
                ("origin", "http://127.0.0.1:4748"),
                ("host", "127.0.0.1:4747"),
            ],
            &[("origin", "null"), ("host", "127.0.0.1:4747")],
            &[("host", "127.0.0.1:4747")],
            &[("origin", "http://127.0.0.1:4747")],
        ];
        for pairs in bad {
            assert!(!same_origin(&headers(pairs)), "{pairs:?}");
        }
    }
}
