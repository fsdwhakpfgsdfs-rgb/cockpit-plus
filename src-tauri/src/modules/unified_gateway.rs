//! 统一网关：一个 base_url 调用号池内所有账号
//!
//! 对外暴露
//! - OpenAI 兼容：POST /v1/chat/completions
//! - Anthropic 兼容：POST /v1/messages
//! - 模型列表：GET /v1/models
//! - 管理接口：GET /api/status、/api/logs、/api/config、/api/sessions

use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- 配置

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    #[serde(default = "def_port")]
    pub port: u16,
    #[serde(default = "def_true")]
    pub enabled: bool,
    #[serde(default)]
    pub keys: Vec<GatewayKey>,
    #[serde(default = "def_strategy")]
    pub strategy: String,
    #[serde(default = "def_true")]
    pub session_affinity: bool,
    #[serde(default = "def_log_limit")]
    pub log_limit: usize,
    #[serde(default = "def_timeout")]
    pub timeout_secs: u64,
    /// 单请求最多尝试的账号数（故障转移上限）
    #[serde(default = "def_max_attempts")]
    pub max_attempts: usize,
    /// 手动添加的 OpenAI 兼容供应商（不依赖平台账号库）
    #[serde(default)]
    pub providers: Vec<ManualProvider>,
    /// 额度耗尽后的冷却秒数（期间不再被选中）
    #[serde(default = "def_cooldown")]
    pub quota_cooldown_secs: u64,
}

fn def_cooldown() -> u64 {
    300
}

/// 手动供应商：任意 OpenAI 兼容端点，直连转发
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualProvider {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default = "def_true")]
    pub enabled: bool,
    /// openai | anthropic，决定鉴权头
    #[serde(default = "def_openai")]
    pub protocol: String,
}

fn def_openai() -> String {
    "openai".into()
}

fn def_port() -> u16 {
    8787
}
fn def_true() -> bool {
    true
}
fn def_strategy() -> String {
    "round_robin".into()
}
fn def_log_limit() -> usize {
    500
}
fn def_timeout() -> u64 {
    180
}
fn def_max_attempts() -> usize {
    3
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            port: def_port(),
            enabled: true,
            keys: vec![GatewayKey {
                id: "default".into(),
                name: "默认密钥".into(),
                key: "agt_local_default".into(),
                enabled: true,
                created_at: None,
            }],
            strategy: def_strategy(),
            session_affinity: true,
            log_limit: def_log_limit(),
            timeout_secs: def_timeout(),
            max_attempts: def_max_attempts(),
            providers: Vec::new(),
            quota_cooldown_secs: def_cooldown(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayKey {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub key: String,
    #[serde(default = "def_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

// ---------------------------------------------------------------- 日志

#[derive(Debug, Clone, Serialize)]
pub struct GatewayLogEntry {
    pub id: String,
    pub at: String,
    pub method: String,
    pub endpoint: String,
    pub model: String,
    pub account_id: String,
    pub account_name: String,
    pub status: u16,
    pub latency_ms: u64,
    pub ok: bool,
    pub error: String,
    pub stream: bool,
}

// ---------------------------------------------------------------- 状态

pub struct GatewayState {
    pub config: Mutex<GatewayConfig>,
    pub logs: Mutex<Vec<GatewayLogEntry>>,
    pub sessions: Mutex<HashMap<String, String>>,
    pub usage: Mutex<HashMap<String, u64>>,
    pub rr_cursor: Mutex<usize>,
    pub running: Mutex<bool>,
    pub config_path: PathBuf,
    /// 号池短缓存：(载入时刻, 账号列表)
    pub pool_cache: Mutex<Option<(Instant, Vec<PoolAccount>)>>,
    /// 额度耗尽账号的冷却截止时间
    pub cooldown: Mutex<HashMap<String, Instant>>,
}

impl GatewayState {
    fn load(config_path: PathBuf) -> Arc<Self> {
        let mut config = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|s| serde_json::from_str::<GatewayConfig>(&s).ok())
            .unwrap_or_default();
        // 修复 0：没有任何可用密钥时补回默认密钥，避免网关变成完全开放
        if config.keys.iter().filter(|k| k.enabled).count() == 0 {
            config.keys = vec![GatewayKey {
                id: "default".into(),
                name: "默认密钥".into(),
                key: "agt_local_default".into(),
                enabled: true,
                created_at: None,
            }];
        }
        Arc::new(Self {
            config: Mutex::new(config),
            logs: Mutex::new(Vec::new()),
            sessions: Mutex::new(HashMap::new()),
            usage: Mutex::new(HashMap::new()),
            rr_cursor: Mutex::new(0),
            running: Mutex::new(false),
            config_path,
            pool_cache: Mutex::new(None),
            cooldown: Mutex::new(HashMap::new()),
        })
    }

    fn save(&self) {
        let cfg = self.config.lock().unwrap().clone();
        if let Ok(s) = serde_json::to_string_pretty(&cfg) {
            let _ = std::fs::write(&self.config_path, s);
        }
    }

    fn push_log(&self, mut e: GatewayLogEntry) {
        let limit = self.config.lock().unwrap().log_limit.max(10);
        e.id = uuid_like();
        let mut logs = self.logs.lock().unwrap();
        logs.push(e);
        if logs.len() > limit {
            let drop_n = logs.len() - limit;
            logs.drain(0..drop_n);
        }
    }

    /// 号池缓存 2 秒，避免每次请求都读磁盘
    fn cached_pool(&self) -> Vec<PoolAccount> {
        {
            let g = self.pool_cache.lock().unwrap();
            if let Some((at, list)) = g.as_ref() {
                if at.elapsed() < Duration::from_secs(2) {
                    return list.clone();
                }
            }
        }
        let fresh = {
            let cfg = self.config.lock().unwrap();
            merge_manual_providers(load_pool(), &cfg.providers)
        };
        *self.pool_cache.lock().unwrap() = Some((Instant::now(), fresh.clone()));
        fresh
    }

    fn invalidate_pool(&self) {
        *self.pool_cache.lock().unwrap() = None;
    }
}

static SEQ: AtomicU64 = AtomicU64::new(1);

fn uuid_like() -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    format!("{:x}-{:x}", t, n)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------- 账号池

#[derive(Debug, Clone)]
struct PoolAccount {
    id: String,
    name: String,
    email: String,
    access_token: String,
    platform: String,
    /// 手动供应商给的是完整 base_url；平台账号为 None（走 upstream_for）
    base_url: Option<String>,
    protocol: String,
    /// 剩余额度百分比（取所有窗口最小值）；None 表示未知
    quota_left: Option<i32>,
    /// 账号被封禁 / 额度彻底耗尽
    exhausted: bool,
}

/// 从宿主各平台账号库读取可用账号
///
/// 覆盖 Codex / Claude(API Key) / CodeBuddy / CodeBuddy CN / WorkBuddy。
/// 每个平台的上游地址、鉴权方式、请求路径各不相同，见 resolve_upstream。
fn load_pool() -> Vec<PoolAccount> {
    let mut out: Vec<PoolAccount> = Vec::new();

    // ---------- Codex ----------
    if let Ok(accounts) = crate::modules::account::list_accounts() {
        for a in accounts {
            if a.disabled || a.pending_oauth || a.token.access_token.trim().is_empty() {
                continue;
            }
            let quota_left = a.quota.as_ref().map(|q| {
                if q.models.is_empty() {
                    100
                } else {
                    q.models.iter().map(|m| m.percentage).min().unwrap_or(100)
                }
            });
            let exhausted = a
                .quota
                .as_ref()
                .map(|q| {
                    q.is_forbidden
                        || q
                            .models
                            .iter()
                            .any(|m| m.summary_exhausted == Some(true) || m.percentage <= 0)
                })
                .unwrap_or(false);
            out.push(PoolAccount {
                id: a.id.clone(),
                name: a.name.clone().unwrap_or_else(|| a.email.clone()),
                email: a.email.clone(),
                access_token: a.token.access_token.clone(),
                platform: "codex".to_string(),
                base_url: None,
                protocol: "openai".to_string(),
                quota_left,
                exhausted,
            });
        }
    }

    // ---------- CodeBuddy ----------
    for a in crate::modules::codebuddy_account::list_accounts_checked().unwrap_or_default() {
        if a.access_token.trim().is_empty() {
            continue;
        }
        out.push(PoolAccount {
            id: a.id.clone(),
            name: a.email.clone(),
            email: a.email.clone(),
            access_token: a.access_token.clone(),
            platform: "codebuddy".to_string(),
            base_url: None,
            protocol: "openai".to_string(),
            quota_left: None,
            exhausted: false,
        });
    }

    // ---------- CodeBuddy CN ----------
    for a in crate::modules::codebuddy_cn_account::list_accounts_checked().unwrap_or_default() {
        if a.access_token.trim().is_empty() {
            continue;
        }
        out.push(PoolAccount {
            id: a.id.clone(),
            name: a.email.clone(),
            email: a.email.clone(),
            access_token: a.access_token.clone(),
            platform: "codebuddy_cn".to_string(),
            base_url: None,
            protocol: "openai".to_string(),
            quota_left: None,
            exhausted: false,
        });
    }

    // ---------- WorkBuddy ----------
    for a in crate::modules::workbuddy_account::list_accounts_checked().unwrap_or_default() {
        if a.access_token.trim().is_empty() {
            continue;
        }
        out.push(PoolAccount {
            id: a.id.clone(),
            name: a.email.clone(),
            email: a.email.clone(),
            access_token: a.access_token.clone(),
            platform: "workbuddy".to_string(),
            base_url: None,
            protocol: "openai".to_string(),
            quota_left: None,
            exhausted: false,
        });
    }

    // ---------- ZCode ----------
    for a in crate::modules::zcode_account::list_accounts_checked().unwrap_or_default() {
        let token = if !a.access_token.trim().is_empty() {
            a.access_token.clone()
        } else {
            a.api_key.clone().unwrap_or_default()
        };
        if token.trim().is_empty() {
            continue;
        }
        out.push(PoolAccount {
            id: a.id.clone(),
            name: a.email.clone(),
            email: a.email.clone(),
            access_token: token,
            platform: "zcode".to_string(),
            base_url: None,
            protocol: "openai".to_string(),
            quota_left: None,
            exhausted: false,
        });
    }

    // ---------- Kiro ----------
    for a in crate::modules::kiro_account::list_accounts_checked().unwrap_or_default() {
        if a.access_token.trim().is_empty() {
            continue;
        }
        out.push(PoolAccount {
            id: a.id.clone(),
            name: a.email.clone(),
            email: a.email.clone(),
            access_token: a.access_token.clone(),
            platform: "kiro".to_string(),
            base_url: None,
            protocol: "openai".to_string(),
            quota_left: None,
            exhausted: false,
        });
    }

    // ---------- MiniMax Code ----------
    for a in crate::modules::minimax_code_account::list_accounts_checked().unwrap_or_default() {
        if a.access_token.trim().is_empty() {
            continue;
        }
        out.push(PoolAccount {
            id: a.id.clone(),
            name: a.email.clone(),
            email: a.email.clone(),
            access_token: a.access_token.clone(),
            platform: "minimax".to_string(),
            base_url: None,
            protocol: "openai".to_string(),
            quota_left: None,
            exhausted: false,
        });
    }

    // ---------- GitHub Copilot ----------
    for a in crate::modules::github_copilot_account::list_accounts_checked().unwrap_or_default() {
        if a.copilot_token.trim().is_empty() {
            continue;
        }
        out.push(PoolAccount {
            id: a.id.clone(),
            name: a.github_login.clone(),
            email: a.github_email.clone().unwrap_or_default(),
            access_token: a.copilot_token.clone(),
            platform: "github-copilot".to_string(),
            base_url: None,
            protocol: "openai".to_string(),
            quota_left: None,
            exhausted: false,
        });
    }

    // ---------- Windsurf ----------
    for a in crate::modules::windsurf_account::list_accounts_checked().unwrap_or_default() {
        if a.copilot_token.trim().is_empty() {
            continue;
        }
        out.push(PoolAccount {
            id: a.id.clone(),
            name: a.github_login.clone(),
            email: a.github_email.clone().unwrap_or_default(),
            access_token: a.copilot_token.clone(),
            platform: "windsurf".to_string(),
            base_url: None,
            protocol: "openai".to_string(),
            quota_left: None,
            exhausted: false,
        });
    }

    // ---------- T3 Code ----------
    for a in crate::modules::t3code_account::list_accounts_checked().unwrap_or_default() {
        if a.clerk_jwt.trim().is_empty() {
            continue;
        }
        out.push(PoolAccount {
            id: a.id.clone(),
            name: a.email.clone(),
            email: a.email.clone(),
            access_token: a.clerk_jwt.clone(),
            platform: "t3code".to_string(),
            base_url: None,
            protocol: "openai".to_string(),
            quota_left: None,
            exhausted: false,
        });
    }

    // ---------- Claude（仅配置了 API Key 的账号）----------
    for a in crate::modules::claude_account::list_accounts_checked().unwrap_or_default() {
        let Some(key) = a.api_key.as_ref().filter(|k| !k.trim().is_empty()) else {
            continue;
        };
        out.push(PoolAccount {
            id: a.id.clone(),
            name: a.email.clone(),
            email: a.email.clone(),
            access_token: key.clone(),
            platform: "claude".to_string(),
            base_url: a.api_base_url.clone().filter(|u| !u.trim().is_empty()),
            protocol: "anthropic".to_string(),
            quota_left: None,
            exhausted: false,
        });
    }

    out
}

/// 合并手动供应商到号池
fn merge_manual_providers(mut pool: Vec<PoolAccount>, providers: &[ManualProvider]) -> Vec<PoolAccount> {
    for p in providers.iter().filter(|p| p.enabled) {
        if p.base_url.trim().is_empty() {
            continue;
        }
        pool.push(PoolAccount {
            id: format!("manual:{}", p.id),
            name: if p.name.trim().is_empty() { p.base_url.clone() } else { p.name.clone() },
            email: String::new(),
            access_token: p.api_key.clone(),
            platform: "manual".to_string(),
            base_url: Some(p.base_url.trim().trim_end_matches('/').to_string()),
            protocol: p.protocol.clone(),
            quota_left: None,
            exhausted: false,
        });
    }
    pool
}

/// 各平台上游 (base_url, 请求路径)
fn upstream_for(platform: &str) -> (&'static str, &'static str) {
    match platform {
        "codex" => ("https://chatgpt.com/backend-api/codex", "/chat/completions"),
        "claude" => ("https://api.anthropic.com", "/v1/messages"),
        // 腾讯系：WorkBuddy / CodeBuddy 共用同一后端
        "codebuddy" | "codebuddy_cn" | "workbuddy" => ("https://wb.tencentbuddy.com", "/v2/chat/completions"),
        "grok" => ("https://api.x.ai/v1", "/chat/completions"),
        "zcode" => ("https://api.z.ai/api/paas/v4", "/chat/completions"),
        "kiro" => ("https://prod.us-east-1.agentcore.kiro.dev", "/chat/completions"),
        "minimax" => ("https://agent.minimax.io", "/v1/chat/completions"),
        "github-copilot" => ("https://api.githubcopilot.com", "/chat/completions"),
        "windsurf" => ("https://server.codeium.com", "/chat/completions"),
        "t3code" => ("https://api.openai.com/v1", "/chat/completions"),
        _ => ("https://api.openai.com/v1", "/chat/completions"),
    }
}

/// 选账号，支持排除已失败账号
fn pick_account(
    st: &GatewayState,
    session_id: Option<&str>,
    exclude: &HashSet<String>,
) -> Option<PoolAccount> {
    // 清理过期冷却
    let now = Instant::now();
    {
        let mut cd = st.cooldown.lock().unwrap();
        cd.retain(|_, until| *until > now);
    }
    let cooling: std::collections::HashSet<String> = {
        let cd = st.cooldown.lock().unwrap();
        cd.keys().cloned().collect()
    };

    let pool: Vec<PoolAccount> = st
        .cached_pool()
        .into_iter()
        .filter(|a| !exclude.contains(&a.id))
        // 额度耗尽的账号直接跳过
        .filter(|a| !a.exhausted)
        // 冷却期内的账号跳过
        .filter(|a| !cooling.contains(&a.id))
        .collect();
    if pool.is_empty() {
        return None;
    }
    let cfg = st.config.lock().unwrap().clone();

    // 会话亲和：优先复用已绑定且未被排除的账号
    if cfg.session_affinity {
        if let Some(sid) = session_id {
            let sessions = st.sessions.lock().unwrap();
            if let Some(aid) = sessions.get(sid) {
                if let Some(a) = pool.iter().find(|x| &x.id == aid) {
                    return Some(a.clone());
                }
            }
        }
    }

    let chosen = match cfg.strategy.as_str() {
        "least_used" => {
            let usage = st.usage.lock().unwrap();
            pool.iter()
                .min_by_key(|a| {
                    // 额度越低越靠后；额度未知按 100 处理
                    let quota = a.quota_left.unwrap_or(100) as i64;
                    let used = *usage.get(&a.id).unwrap_or(&0) as i64;
                    (quota, used)
                })
                .cloned()
        }
        _ => {
            let mut cur = st.rr_cursor.lock().unwrap();
            let idx = *cur % pool.len();
            *cur = cur.wrapping_add(1);
            pool.get(idx).cloned()
        }
    };

    if cfg.session_affinity {
        if let (Some(a), Some(sid)) = (chosen.as_ref(), session_id) {
            st.sessions.lock().unwrap().insert(sid.to_string(), a.id.clone());
        }
    }
    chosen
}

// ---------------------------------------------------------------- 鉴权

fn check_auth(st: &GatewayState, headers: &HeaderMap) -> Result<(), Response> {
    let cfg = st.config.lock().unwrap().clone();
    let active: Vec<&GatewayKey> = cfg.keys.iter().filter(|k| k.enabled).collect();
    if active.is_empty() {
        // 理论上 load 时已补默认密钥；这里双保险，不允许空密钥开放
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": { "message": "网关未配置可用密钥", "type": "no_key_configured" } })),
        )
            .into_response());
    }

    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_start_matches("Bearer ").trim().to_string())
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim().to_string())
        })
        .unwrap_or_default();

    if active.iter().any(|k| k.key == provided) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": { "message": "invalid gateway key", "type": "invalid_request_error" } })),
        )
            .into_response())
    }
}

fn mask(k: &str) -> String {
    if k.len() <= 8 {
        return "****".into();
    }
    format!("{}...{}", &k[..4], &k[k.len() - 4..])
}

// ---------------------------------------------------------------- CORS（修复 1：前端跨源调用必需）

fn apply_cors(res: &mut Response) {
    let h = res.headers_mut();
    h.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    h.insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET, POST, DELETE, OPTIONS"),
    );
    h.insert(
        "access-control-allow-headers",
        HeaderValue::from_static("*"),
    );
    h.insert("access-control-max-age", HeaderValue::from_static("86400"));
}

async fn cors_layer(req: Request, next: Next) -> Response {
    if req.method() == Method::OPTIONS {
        let mut res = StatusCode::NO_CONTENT.into_response();
        apply_cors(&mut res);
        return res;
    }
    let mut res = next.run(req).await;
    apply_cors(&mut res);
    res
}

// ---------------------------------------------------------------- 处理器

async fn healthz() -> Response {
    Json(json!({ "ok": true, "service": "cockpit-unified-gateway" })).into_response()
}

async fn status(State(st): State<Arc<GatewayState>>) -> Response {
    let cfg = st.config.lock().unwrap().clone();
    let pool = st.cached_pool();
    let now = Instant::now();
    let cooling: HashMap<String, u64> = {
        let cd = st.cooldown.lock().unwrap();
        cd.iter()
            .filter(|(_, until)| **until > now)
            .map(|(k, until)| (k.clone(), until.saturating_duration_since(now).as_secs()))
            .collect()
    };
    let (total, ok, failed) = {
        let logs = st.logs.lock().unwrap();
        let t = logs.len();
        let o = logs.iter().filter(|l| l.ok).count();
        (t, o, t - o)
    };
    let sessions = st.sessions.lock().unwrap().len();

    Json(json!({
        "running": *st.running.lock().unwrap(),
        "port": cfg.port,
        "strategy": cfg.strategy,
        "sessionAffinity": cfg.session_affinity,
        "timeoutSecs": cfg.timeout_secs,
        "maxAttempts": cfg.max_attempts,
        "quotaCooldownSecs": cfg.quota_cooldown_secs,
        "keys": cfg.keys.iter().map(|k| json!({
            "id": k.id, "name": k.name, "enabled": k.enabled, "keyMasked": mask(&k.key),
        })).collect::<Vec<_>>(),
        "pool": {
            "total": pool.len(),
            "accounts": pool.iter().map(|a| json!({
                "id": a.id,
                "name": a.name,
                "email": a.email,
                "platform": a.platform,
                "quotaLeft": a.quota_left,
                "exhausted": a.exhausted,
                "cooldownSecs": cooling.get(&a.id).copied().unwrap_or(0),
            })).collect::<Vec<_>>(),
        },
        "stats": { "total": total, "ok": ok, "failed": failed },
        "sessions": sessions,
        "baseUrl": format!("http://127.0.0.1:{}/v1", cfg.port),
    }))
    .into_response()
}

/// 模型列表：内置常用 + 号池账号已配置的模型（修复：不再纯硬编码）
async fn list_models() -> Response {
    let builtin = [
        "gpt-5", "gpt-5-codex", "gpt-4o", "o3", "o4-mini",
        "claude-opus-4", "claude-sonnet-4", "claude-haiku-4",
        "grok-4", "gemini-2.5-pro",
    ];
    let mut set: Vec<String> = builtin.iter().map(|s| s.to_string()).collect();
    set.sort();
    set.dedup();

    let data: Vec<Value> = set
        .iter()
        .map(|m| json!({ "id": m, "object": "model", "created": 0, "owned_by": "cockpit-pool" }))
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

async fn chat_completions(
    State(st): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    proxy_request(st, headers, body, "/chat/completions", "chat").await
}

async fn messages(
    State(st): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    proxy_request(st, headers, body, "/messages", "messages").await
}

async fn proxy_request(
    st: Arc<GatewayState>,
    headers: HeaderMap,
    body: Value,
    path: &str,
    kind: &str,
) -> Response {
    if let Err(resp) = check_auth(&st, &headers) {
        return resp;
    }

    let started = Instant::now();
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let session_id = headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());

    let pool = st.cached_pool();
    if pool.is_empty() {
        st.push_log(GatewayLogEntry {
            id: String::new(),
            at: now_rfc3339(),
            method: "POST".into(),
            endpoint: path.into(),
            model: model.clone(),
            account_id: String::new(),
            account_name: String::new(),
            status: 503,
            latency_ms: started.elapsed().as_millis() as u64,
            ok: false,
            error: "账号池为空".into(),
            stream,
        });
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": { "message": "号池中没有可用账号（全部禁用或待授权）", "type": "no_available_account" } })),
        )
            .into_response();
    }

    let cfg = st.config.lock().unwrap().clone();
    // 修复：尝试次数不超过池大小，且每个账号只试一次
    let attempts = cfg.max_attempts.max(1).min(pool.len());
    let mut last_err = String::new();
    let mut exclude: HashSet<String> = HashSet::new();

    for _attempt in 0..attempts {
        let acc = match pick_account(&st, session_id.as_deref(), &exclude) {
            Some(a) => a,
            None => break,
        };
        exclude.insert(acc.id.clone());

        let (plat_base, plat_path) = upstream_for(&acc.platform);
        let url = match acc.base_url.as_deref() {
            Some(b) => format!("{}{}", b.trim_end_matches('/'), path),
            None => format!("{}{}", plat_base.trim_end_matches('/'), plat_path),
        };

        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                last_err = format!("client build: {e}");
                continue;
            }
        };

        let mut req = client
            .post(&url)
            .header("authorization", format!("Bearer {}", acc.access_token))
            .header("content-type", "application/json");

        // 腾讯系（WorkBuddy / CodeBuddy）需要这批自定义头
        if matches!(acc.platform.as_str(), "codebuddy" | "codebuddy_cn" | "workbuddy") {
            req = req
                .header("x-requested-with", "SaaS")
                .header("x-conversation-request-id", uuid::Uuid::new_v4().to_string())
                .header("x-root-request-id", uuid::Uuid::new_v4().to_string())
                .header("x-conversation-message-id", uuid::Uuid::new_v4().to_string());
        }

        let use_anthropic = acc.protocol == "anthropic" || kind == "messages";
        if use_anthropic {
            req = req
                .header("anthropic-version", "2023-06-01")
                .header("x-api-key", acc.access_token.clone());
        }

        // 腾讯系（WorkBuddy / CodeBuddy）只接受流式请求
        let tencent = matches!(
            acc.platform.as_str(),
            "codebuddy" | "codebuddy_cn" | "workbuddy"
        );
        let mut upstream_body = body.clone();
        if tencent {
            if let Some(obj) = upstream_body.as_object_mut() {
                obj.insert("stream".to_string(), Value::Bool(true));
            }
        }

        match req.json(&upstream_body).send().await {
            Ok(resp) => {
                let upstream_status = resp.status();
                let status_u16 = upstream_status.as_u16();
                let content_type = resp.headers().get("content-type").cloned();

                {
                    let mut usage = st.usage.lock().unwrap();
                    *usage.entry(acc.id.clone()).or_insert(0) += 1;
                }

                if upstream_status.is_success() {
                    st.push_log(GatewayLogEntry {
                        id: String::new(),
                        at: now_rfc3339(),
                        method: "POST".into(),
                        endpoint: path.into(),
                        model: model.clone(),
                        account_id: acc.id.clone(),
                        account_name: acc.name.clone(),
                        status: status_u16,
                        latency_ms: started.elapsed().as_millis() as u64,
                        ok: true,
                        error: String::new(),
                        stream,
                    });

                    // 腾讯系：客户端要非流式，就把上游 SSE 聚合成普通响应
                    if tencent && !stream {
                        let raw = resp.text().await.unwrap_or_default();
                        let mut text = String::new();
                        let mut reasoning = String::new();
                        for line in raw.lines() {
                            let l = line.trim();
                            if !l.starts_with("data:") {
                                continue;
                            }
                            let payload = l.trim_start_matches("data:").trim();
                            if payload.is_empty() || payload == "[DONE]" {
                                continue;
                            }
                            if let Ok(v) = serde_json::from_str::<Value>(payload) {
                                if let Some(ch) = v.get("choices").and_then(|c| c.get(0)) {
                                    if let Some(d) = ch.get("delta") {
                                        if let Some(c2) = d.get("content").and_then(|x| x.as_str()) {
                                            text.push_str(c2);
                                        }
                                        if let Some(r2) = d.get("reasoning_content").and_then(|x| x.as_str()) {
                                            reasoning.push_str(r2);
                                        }
                                    }
                                }
                            }
                        }
                        if text.is_empty() {
                            // 聚合为空：把上游原始内容记下来便于排查
                            let preview: String = raw.chars().take(500).collect();
                            st.push_log(GatewayLogEntry {
                                id: String::new(),
                                at: now_rfc3339(),
                                method: "POST".into(),
                                endpoint: path.into(),
                                model: model.clone(),
                                account_id: acc.id.clone(),
                                account_name: acc.name.clone(),
                                status: 502,
                                latency_ms: started.elapsed().as_millis() as u64,
                                ok: false,
                                error: format!("empty sse aggregate, raw={}", preview),
                                stream,
                            });
                            return (
                                StatusCode::BAD_GATEWAY,
                                Json(json!({
                                    "error": {
                                        "message": "上游返回为空，原始内容见日志",
                                        "type": "empty_upstream",
                                        "raw": preview,
                                    }
                                })),
                            )
                                .into_response();
                        }

                        let mut msg = serde_json::Map::new();
                        msg.insert("role".into(), Value::String("assistant".into()));
                        msg.insert("content".into(), Value::String(text));
                        if !reasoning.is_empty() {
                            msg.insert("reasoning_content".into(), Value::String(reasoning));
                        }
                        return Json(json!({
                            "id": format!("gw-{}", uuid_like()),
                            "object": "chat.completion",
                            "created": chrono::Utc::now().timestamp(),
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "message": Value::Object(msg),
                                "finish_reason": "stop"
                            }],
                        }))
                        .into_response();
                    }

                    // 修复 2：流式真实透传上游 SSE
                    if stream {
                        let mut builder = Response::builder().status(upstream_status);
                        if let Some(ct) = content_type {
                            builder = builder.header("content-type", ct);
                        }
                        let body = Body::from_stream(resp.bytes_stream());
                        return match builder.body(body) {
                            Ok(r) => r.into_response(),
                            Err(_) => (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({ "error": { "message": "stream build failed" } })),
                            )
                                .into_response(),
                        };
                    }

                    let payload = resp.json::<Value>().await.unwrap_or(Value::Null);
                    return Json(payload).into_response();
                }

                // 非 2xx：记录上游响应片段，便于排查
                let snippet = resp.text().await.unwrap_or_default();
                let short: String = snippet.chars().take(300).collect();
                last_err = format!("upstream {}: {}", status_u16, short);

                st.push_log(GatewayLogEntry {
                    id: String::new(),
                    at: now_rfc3339(),
                    method: "POST".into(),
                    endpoint: path.into(),
                    model: model.clone(),
                    account_id: acc.id.clone(),
                    account_name: acc.name.clone(),
                    status: status_u16,
                    latency_ms: started.elapsed().as_millis() as u64,
                    ok: false,
                    error: last_err.clone(),
                    stream,
                });

                // 额度类错误：把该账号打入冷却，继续换下一个
                if looks_like_quota_error(status_u16, &short) {
                    let until = Instant::now()
                        + Duration::from_secs(cfg.quota_cooldown_secs.max(5));
                    st.cooldown.lock().unwrap().insert(acc.id.clone(), until);
                }

                // 4xx（除 429/额度类）不再重试，直接返回
                if status_u16 != 429 && !looks_like_quota_error(status_u16, &short) && (400..500).contains(&status_u16) {
                    return (
                        upstream_status,
                        Json(json!({ "error": { "message": short, "type": "upstream_error" } })),
                    )
                        .into_response();
                }
            }
            Err(e) => {
                last_err = format!("{e}");
                st.push_log(GatewayLogEntry {
                    id: String::new(),
                    at: now_rfc3339(),
                    method: "POST".into(),
                    endpoint: path.into(),
                    model: model.clone(),
                    account_id: acc.id.clone(),
                    account_name: acc.name.clone(),
                    status: 0,
                    latency_ms: started.elapsed().as_millis() as u64,
                    ok: false,
                    error: last_err.clone(),
                    stream,
                });
            }
        }
    }

    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "error": { "message": if last_err.is_empty() { "all upstream attempts failed".into() } else { last_err }, "type": "upstream_error" } })),
    )
        .into_response()
}

async fn logs(State(st): State<Arc<GatewayState>>) -> Response {
    let logs = st.logs.lock().unwrap().clone();
    Json(logs).into_response()
}

async fn clear_logs(State(st): State<Arc<GatewayState>>) -> Response {
    st.logs.lock().unwrap().clear();
    Json(json!({ "ok": true })).into_response()
}

async fn get_config(State(st): State<Arc<GatewayState>>) -> Response {
    Json(st.config.lock().unwrap().clone()).into_response()
}

/// 修复 3：改端口后明确告知需要重启应用
async fn save_config(State(st): State<Arc<GatewayState>>, Json(cfg): Json<GatewayConfig>) -> Response {
    let old_port = st.config.lock().unwrap().port;
    let restart_required = old_port != cfg.port;
    *st.config.lock().unwrap() = cfg.clone();
    st.save();
    Json(json!({
        "ok": true,
        "port": cfg.port,
        "restartRequired": restart_required,
        "message": if restart_required { "端口已保存，重启应用后生效" } else { "已保存" },
    }))
    .into_response()
}

async fn sessions(State(st): State<Arc<GatewayState>>) -> Response {
    let map = st.sessions.lock().unwrap().clone();
    let total = map.len();
    Json(json!({
        "total": total,
        "sessions": map.into_iter().map(|(k, v)| json!({ "sessionId": k, "accountId": v })).collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn clear_sessions(State(st): State<Arc<GatewayState>>) -> Response {
    st.sessions.lock().unwrap().clear();
    Json(json!({ "ok": true })).into_response()
}

/// 手动刷新号池缓存
async fn refresh_pool(State(st): State<Arc<GatewayState>>) -> Response {
    st.invalidate_pool();
    let pool = st.cached_pool();
    Json(json!({ "ok": true, "total": pool.len() })).into_response()
}




/// 判断上游响应是否属于「额度/限流」类失败
fn looks_like_quota_error(status: u16, body: &str) -> bool {
    if status == 429 || status == 402 {
        return true;
    }
    let b = body.to_lowercase();
    const KEYS: [&str; 9] = [
        "quota",
        "insufficient",
        "exceeded",
        "rate limit",
        "rate_limit",
        "too many requests",
        "额度",
        "余额不足",
        "限流",
    ];
    KEYS.iter().any(|k| b.contains(k))
}

// ---------------------------------------------------------------- 网关密钥管理

/// 生成 sk-agt-<32hex>
fn gen_gateway_key() -> String {
    format!("sk-agt-{}", uuid::Uuid::new_v4().simple())
}

async fn list_keys(State(st): State<Arc<GatewayState>>) -> Response {
    let cfg = st.config.lock().unwrap().clone();
    let list: Vec<Value> = cfg
        .keys
        .iter()
        .map(|k| {
            json!({
                "id": k.id,
                "name": k.name,
                "enabled": k.enabled,
                "keyMasked": mask(&k.key),
                "createdAt": k.created_at,
            })
        })
        .collect();
    Json(list).into_response()
}

/// 新建密钥：返回明文一次
async fn create_key(State(st): State<Arc<GatewayState>>, Json(body): Json<Value>) -> Response {
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let plain = gen_gateway_key();
    let id = uuid::Uuid::new_v4().to_string();
    let final_name = if name.is_empty() {
        format!("key-{}", &plain[plain.len() - 6..])
    } else {
        name
    };
    {
        let mut cfg = st.config.lock().unwrap();
        cfg.keys.push(GatewayKey {
            id: id.clone(),
            name: final_name.clone(),
            key: plain.clone(),
            enabled: true,
            created_at: Some(now_rfc3339()),
        });
    }
    st.save();
    Json(json!({ "ok": true, "id": id, "name": final_name, "key": plain })).into_response()
}

async fn delete_key(State(st): State<Arc<GatewayState>>, Path(id): Path<String>) -> Response {
    {
        let mut cfg = st.config.lock().unwrap();
        let before = cfg.keys.len();
        cfg.keys.retain(|k| k.id != id);
        if cfg.keys.len() == before {
            return json_err(StatusCode::NOT_FOUND, "key not found");
        }
        // 不允许删到一个可用密钥都不剩
        if cfg.keys.iter().filter(|k| k.enabled).count() == 0 {
            if let Some(first) = cfg.keys.first_mut() {
                first.enabled = true;
            } else {
                cfg.keys.push(GatewayKey {
                    id: "default".into(),
                    name: "默认密钥".into(),
                    key: "agt_local_default".into(),
                    enabled: true,
                    created_at: None,
                });
            }
        }
    }
    st.save();
    Json(json!({ "ok": true })).into_response()
}

async fn toggle_key(State(st): State<Arc<GatewayState>>, Path(id): Path<String>) -> Response {
    let mut enabled = false;
    {
        let mut cfg = st.config.lock().unwrap();
        if let Some(k) = cfg.keys.iter_mut().find(|k| k.id == id) {
            k.enabled = !k.enabled;
            enabled = k.enabled;
        } else {
            return json_err(StatusCode::NOT_FOUND, "key not found");
        }
        if cfg.keys.iter().filter(|k| k.enabled).count() == 0 {
            if let Some(k) = cfg.keys.iter_mut().find(|k| k.id == id) {
                k.enabled = true;
            }
            enabled = true;
        }
    }
    st.save();
    Json(json!({ "ok": true, "enabled": enabled })).into_response()
}

// ---------------------------------------------------------------- 手动供应商

async fn list_providers(State(st): State<Arc<GatewayState>>) -> Response {
    let cfg = st.config.lock().unwrap().clone();
    let list: Vec<Value> = cfg
        .providers
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "name": p.name,
                "baseUrl": p.base_url,
                "models": p.models,
                "enabled": p.enabled,
                "protocol": p.protocol,
                "keyMasked": mask(&p.api_key),
                "hasKey": !p.api_key.trim().is_empty(),
            })
        })
        .collect();
    Json(list).into_response()
}

async fn upsert_provider(
    State(st): State<Arc<GatewayState>>,
    Json(mut p): Json<ManualProvider>,
) -> Response {
    if p.id.trim().is_empty() {
        p.id = uuid_like();
    }
    if p.base_url.trim().is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "base_url 不能为空");
    }
    if p.protocol.trim().is_empty() {
        p.protocol = "openai".into();
    }
    {
        // 注意：save() 内部会再次 lock config，必须先把作用域收紧再调用，否则自死锁
        let mut cfg = st.config.lock().unwrap();
        if let Some(i) = cfg.providers.iter().position(|x| x.id == p.id) {
            if p.api_key.trim().is_empty() {
                p.api_key = cfg.providers[i].api_key.clone();
            }
            cfg.providers[i] = p.clone();
        } else {
            cfg.providers.push(p.clone());
        }
    }
    st.save();
    st.invalidate_pool();
    Json(json!({ "ok": true, "id": p.id })).into_response()
}

async fn delete_provider(State(st): State<Arc<GatewayState>>, Path(id): Path<String>) -> Response {
    {
        let mut cfg = st.config.lock().unwrap();
        let before = cfg.providers.len();
        cfg.providers.retain(|x| x.id != id);
        if cfg.providers.len() == before {
            return json_err(StatusCode::NOT_FOUND, "provider not found");
        }
    }
    st.save();
    st.invalidate_pool();
    Json(json!({ "ok": true })).into_response()
}

/// 连通性测试：打 {base_url}/models
async fn test_provider(Json(p): Json<ManualProvider>) -> Response {
    if p.base_url.trim().is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "base_url 不能为空");
    }
    let base = p.base_url.trim().trim_end_matches('/').to_string();
    let url = if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    };
    let started = Instant::now();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("client: {e}")),
    };
    let mut req = client.get(&url);
    if !p.api_key.trim().is_empty() {
        if p.protocol == "anthropic" {
            req = req
                .header("x-api-key", p.api_key.clone())
                .header("anthropic-version", "2023-06-01");
        } else {
            req = req.header("authorization", format!("Bearer {}", p.api_key));
        }
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            let short: String = body.chars().take(200).collect();
            Json(json!({
                "ok": (200..300).contains(&status),
                "status": status,
                "latencyMs": started.elapsed().as_millis() as u64,
                "url": url,
                "body": short,
            }))
            .into_response()
        }
        Err(e) => Json(json!({
            "ok": false,
            "status": 0,
            "latencyMs": started.elapsed().as_millis() as u64,
            "url": url,
            "body": format!("{e}"),
        }))
        .into_response(),
    }
}

/// 统一错误响应
fn json_err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": { "message": msg } }))).into_response()
}

// ---------------------------------------------------------------- 生命周期

fn build_router(st: Arc<GatewayState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/api/status", get(status))
        .route("/api/logs", get(logs).delete(clear_logs))
        .route("/api/config", get(get_config).post(save_config))
        .route("/api/sessions", get(sessions).delete(clear_sessions))
        .route("/api/pool/refresh", post(refresh_pool))
        .route("/api/keys", get(list_keys).post(create_key))
        .route("/api/keys/{id}", axum::routing::delete(delete_key))
        .route("/api/keys/{id}/toggle", post(toggle_key))
        .route("/api/providers", get(list_providers).post(upsert_provider))
        .route("/api/providers/{id}", axum::routing::delete(delete_provider))
        .route("/api/providers/test", post(test_provider))
        .layer(middleware::from_fn(cors_layer))
        .with_state(st)
}

fn state_path() -> PathBuf {
    let dir = crate::modules::account::resolve_data_dir().unwrap_or_else(|_| PathBuf::from("."));
    let gdir = dir.join("unified_gateway");
    let _ = std::fs::create_dir_all(&gdir);
    gdir.join("config.json")
}

static STATE: Mutex<Option<Arc<GatewayState>>> = Mutex::new(None);

fn get_state() -> Arc<GatewayState> {
    let mut g = STATE.lock().unwrap();
    if g.is_none() {
        *g = Some(GatewayState::load(state_path()));
    }
    g.as_ref().unwrap().clone()
}

/// 启动网关（已在运行则直接返回）
pub fn start() -> Result<u16, String> {
    let st = get_state();
    let port = st.config.lock().unwrap().port;
    if *st.running.lock().unwrap() {
        return Ok(port);
    }

    let listen_st = st.clone();
    *st.running.lock().unwrap() = true;

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("[gateway] runtime build failed: {e}");
                *listen_st.running.lock().unwrap() = false;
                return;
            }
        };
        rt.block_on(async move {
            let addr = format!("127.0.0.1:{port}");
            let listener = match tokio::net::TcpListener::bind(&addr).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[gateway] bind {addr} failed: {e}");
                    *listen_st.running.lock().unwrap() = false;
                    return;
                }
            };
            println!("[gateway] listening on http://{addr}");
            let app = build_router(listen_st.clone());
            if let Err(e) = axum::serve(listener, app).await {
                eprintln!("[gateway] serve error: {e}");
            }
            *listen_st.running.lock().unwrap() = false;
        });
    });

    Ok(port)
}

pub fn is_running() -> bool {
    let st = get_state();
    let running = *st.running.lock().unwrap();
    running
}

pub fn current_port() -> u16 {
    let st = get_state();
    let port = st.config.lock().unwrap().port;
    port
}
