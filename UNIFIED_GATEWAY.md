# 统一网关（Unified Gateway）

基于 **cockpit-tools v1.5.2** 的增量模块。把应用内已登录的各平台账号聚合成一个
OpenAI / Anthropic 兼容的 HTTP 端口，一个地址调用全部账号，**不依赖任何第三方中转**。

---

## 1. 它解决什么

cockpit-tools 原本每个平台只能在本应用里切换账号使用。本模块补上的是：

```
任意 OpenAI SDK / Cursor / 脚本
        │
        ▼  http://127.0.0.1:8787/v1
   ┌─────────────────────────┐
   │  统一网关（本模块）        │
   │  · 号池聚合              │
   │  · 额度感知选路           │
   │  · 额度冷却 / 故障转移     │
   │  · 会话亲和              │
   │  · 请求日志              │
   └─────────────────────────┘
        │  直读应用自带加密账号库并解密
        ▼  按平台走各自真实上游
   Codex / Claude / CodeBuddy / WorkBuddy / ...
```

---

## 2. 快速开始

应用启动后网关自动拉起（`lib.rs` 的 setup 线程，端口来自配置文件）。

```bash
# 健康检查
curl http://127.0.0.1:8787/healthz

# 号池状态
curl http://127.0.0.1:8787/api/status
```

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:8787/v1",
    api_key="<在网关页「网关密钥 → 新建」生成的 sk-agt-...>",
)

print(client.chat.completions.create(
    model="auto",
    messages=[{"role": "user", "content": "你好"}],
).choices[0].message.content)

# 流式
for chunk in client.chat.completions.create(
    model="auto",
    messages=[{"role": "user", "content": "你好"}],
    stream=True,
):
    print(chunk.choices[0].delta.content or "", end="")
```

---

## 3. 配置文件

位置：`%USERPROFILE%\.cockpit_tools\unified_gateway\config.json`

```json
{
  "port": 8787,
  "enabled": true,
  "keys": [{ "id": "default", "name": "默认密钥", "key": "agt_local_default", "enabled": true }],
  "strategy": "least_used",
  "session_affinity": true,
  "log_limit": 500,
  "timeout_secs": 180,
  "max_attempts": 3,
  "quota_cooldown_secs": 300,
  "proxy": "http://127.0.0.1:10808",
  "platform_proxies": { "codebuddy_cn": "", "workbuddy": "", "codex": "http://127.0.0.1:10808" },
  "account_proxies": { "codex_account_abc123": "socks5://127.0.0.1:10809" },
  "providers": []
}
```

| 字段 | 说明 |
|---|---|
| `port` | 监听端口，改后需重启应用 |
| `strategy` | `round_robin` 轮询 / `least_used` 按额度优先 |
| `session_affinity` | 同一 `x-session-id` 固定同一账号 |
| `timeout_secs` | 单次上游请求超时 |
| `max_attempts` | 故障转移最多尝试的账号数 |
| `quota_cooldown_secs` | 额度类错误后的冷却时长 |
| `proxy` | 默认出站代理；为空则直连 |
| `platform_proxies` | 按平台覆盖代理，见 3.1 |
| `account_proxies` | 按账号覆盖代理（键为号池账号 id），优先级最高，见 3.1 |
| `providers` | 手动添加的 OpenAI 兼容端点（可选） |

### 3.1 出站代理（三级覆盖）

按优先级从高到低解析，**空字符串表示强制直连**：

```
1. 账号自带代理        手动供应商单独配置的 proxy
2. 账号覆盖代理        account_proxies["<account_id>"]
3. 平台覆盖代理        platform_proxies["<platform>"]
4. 全局默认代理        proxy
5. 都没有              → 直连（显式 .no_proxy()）
```

**界面输入约定**（配置弹窗）：

| 输入框内容 | 提交值 | 含义 |
|---|---|---|
| 留空 | 不提交该键 | 沿用上一级 |
| `direct` | `""` | 强制直连 |
| `http://...` / `socks5://...` | 原值 | 使用该代理 |

> **直连必须显式关闭环境变量代理**：reqwest 默认会读 `HTTP_PROXY` / `HTTPS_PROXY`，
> 所以「不设代理」并不等于直连。`client_for` 在解析结果为 `None` 时会调用
> `.no_proxy()`，否则强制直连的平台上会静默走环境变量里的代理。

```rust
fn resolve_proxy(cfg, platform, account_proxy) -> Option<String> {
    if let Some(p) = account_proxy { ... }                      // 账号自带
    if let Some(v) = cfg.platform_proxies.get(platform) { ... }  // 平台覆盖
    cfg.proxy.filter(|s| !s.trim().is_empty())                   // 全局
}
```

| 场景 | 配置 |
|---|---|
| 全机必须走代理 | 只填 `proxy`，`platform_proxies` 留空 |
| 国内平台直连、国外走代理 | 给国内平台填 `""`，其余沿用 `proxy` |
| 不同平台用不同代理 | 各自填不同地址 |
| 某平台要走固定出口 IP | 单独填该出口 |
| 同平台里个别账号要走不同出口 | `account_proxies["<account_id>"]` |

**客户端缓存**：`GatewayState.clients: Mutex<HashMap<String, reqwest::Client>>`，
按 `"<proxy>|<timeout>"` 复用，避免每个请求重建 TLS。

> 注意：如果整机直连不通（国内域名也连不上），不要给国内平台填空 —— 
> v2rayN 之类的代理本身按规则分流，你在网关层再强制直连只会把请求打回死路。

---

## 4. 路由策略

### `round_robin`
按号池顺序循环，不关心额度。

### `least_used`（推荐）
按 `(-剩余额度, 已用次数)` 升序取最小，即：

1. **剩余额度越高越优先**
2. 同额度内按已用次数轮转

实测（16 个号，额度 4%~100%）：

```
192****5351   quota=100   ← 被选中
130****7407   quota=100   ← 交替
192****5351   quota=100
130****7407   quota=100
```

额度 4%/5%/6% 的三个号一次都没被选。

> 注意：曾有一版把排序键写成 `(quota, used)`，`min_by_key` 会取到**最低**额度，
> 导致专挑快耗尽的号。已修为 `(-quota, used)`。

---

## 5. 额度感知与自动切号

三层机制：

### 5.1 静态排除
选路前直接过滤：

- `quota_left == 0` 或 `exhausted == true`
- `status` 为 `banned` / `forbidden` / `disabled` / `invalid` / `expired` / `suspended`
- 在冷却表中的账号

### 5.2 运行时冷却
上游返回 `429` / `402`，或响应体命中
`quota` / `insufficient` / `exceeded` / `rate limit` / `额度` / `余额不足` / `限流`
→ 该账号进入 `quota_cooldown_secs` 冷却，自动换下一个。

### 5.3 额度来源
- **Codex**：`quota.models[].percentage`、`summary_exhausted`、`is_forbidden`
- **WorkBuddy / CodeBuddy**：解析账号库缓存的 `quota_raw`（零额外 API 调用）
  ```
  quota_raw.userResource → Response.Data.Accounts[] 或 resources[]
  总量：SlicePeriodCapacitySizePrecise / CycleCapacitySizePrecise / CapacitySize
  剩余：SlicePeriodCapacityRemainPrecise / CycleCapacityRemainPrecise / CapacityRemain
  Unlimited: true → 100%
  ```
- **其余平台**：`null`（未知，按 100 处理）

额度缓存为空时，在应用对应平台页点「刷新全部」即可填充。

---

## 6. 支持的平台

| 平台 | 账号源 | 上游 | 协议状态 |
|---|---|---|---|
| Codex | `account` | `chatgpt.com/backend-api/codex` | 标准 |
| Claude | `claude_account`（API Key 账号） | `api.anthropic.com/v1/messages` | 标准（Anthropic） |
| CodeBuddy | `codebuddy_account` | `wb.tencentbuddy.com/v2/chat/completions` | **实测通过** |
| CodeBuddy CN | `codebuddy_cn_account` | 同上 | **实测通过** |
| WorkBuddy | `workbuddy_account` | 同上 | **实测通过** |
| ZCode | `zcode_account` | `api.z.ai/api/paas/v4` | 待验证 |
| Kiro | `kiro_account` | `prod.us-east-1.agentcore.kiro.dev` | 待验证 |
| MiniMax Code | `minimax_code_account` | `agent.minimax.io` | 待验证 |
| GitHub Copilot | `github_copilot_account`（`copilot_token`） | `api.githubcopilot.com` | 待验证 |
| Windsurf | `windsurf_account`（`copilot_token`） | `server.codeium.com` | 待验证 |
| T3 Code | `t3code_account`（`clerk_jwt`） | 中转 | 待验证 |

**接不了**：Grok（`GrokAccountView` 的 token 是占位符，源码注明 real credentials never cross IPC）、
Qoder / Zed（结构体无 token 字段）、Cursor / Trae（无 `list_accounts` 导出）。

### 6.1 模型目录

`/v1/models` **按号池里实际存在的平台动态返回**，不再硬编码。

腾讯系（CodeBuddy / CodeBuddy CN / WorkBuddy）账号可调 **36 个模型**：

**国内池（19）**

```
auto            glm-5.4         glm-5.3          glm-5.3-flash
glm-5.2         kimi-k3-1       kimi-k2.8-preview kimi-k3.1-pro
deepseek-v4.1-flash             deepseek-v4-pro   space-bunny
step-5-preview  mimo-v2.6-pro   minimax-m3.1-flash
hy4std          hy4flash-free   hy4flash
hunyuan-image-alpha             hunyuan-image-alpha-edit
```

**国际池（17）**

```
max-model       deepseek-v4.1-flash-sg
gpt-6.2-astra   gpt-6-astra     gpt-6.2-sol      gpt-6.1-sol
gpt-6-sol       gpt-6.1-terra   gpt-6-luna
gpt-5.6-sol     gpt-5.6-terra   gpt-5.6-luna
grok-4.7        gemini-3.8-flash
kimi-k3.1-pro-highspeed         kimi-k3     hy-image-3.5
```

号池里加了 Codex 账号会追加 `gpt-5` / `gpt-5-codex` / `o3` / `o4-mini`，
加了 Claude 账号会追加 `claude-opus-4` / `claude-sonnet-4` / `claude-haiku-4`。


### 6.2 腾讯系的两个关键细节

1. **私有请求头**
   ```
   X-Requested-With: SaaS
   X-Conversation-Request-ID: <uuid>
   X-Root-Request-ID: <uuid>
   X-Conversation-Message-ID: <uuid>
   ```
2. **只接受流式** —— 非流式会被拒（错误码 `11101 Non-stream chat request is currently not supported`）
   → 网关对腾讯系强制 `stream: true` 发上游；
   客户端若请求非流式，网关把 SSE 拆包聚合回标准 `chat.completion`。

---

## 7. HTTP 接口

### 网关调用

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/healthz` | 健康检查 |
| GET | `/v1/models` | 模型列表 |
| POST | `/v1/chat/completions` | OpenAI 兼容对话（支持 `stream`） |
| POST | `/v1/messages` | Anthropic 兼容对话 |

鉴权：`Authorization: Bearer <网关密钥>`；Anthropic 路径也接受 `x-api-key`。

### 管理接口

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/status` | 号池 / 统计 / 密钥 / 额度 / 冷却 |
| GET · DELETE | `/api/logs` | 请求日志 / 清空 |
| GET · POST | `/api/config` | 读取 / 保存配置 |
| GET · DELETE | `/api/sessions` | 会话绑定 / 清空 |
| POST | `/api/pool/refresh` | 刷新号池缓存 |
| GET · POST | `/api/keys` | 密钥列表（脱敏）/ 新建（明文仅返回一次） |
| DELETE | `/api/keys/{id}` | 删除密钥 |
| POST | `/api/keys/{id}/toggle` | 停用 / 启用 |
| GET · POST | `/api/providers` | 手动供应商列表 / 新增或更新 |
| DELETE | `/api/providers/{id}` | 删除 |
| POST | `/api/providers/test` | 连通性测试（打 `{base_url}/models`） |

密钥格式：`sk-agt-<32 位 hex>`，明文只在创建时返回一次。

---

## 8. 构建

本机 16GB 内存编不动这个 87 万行 Rust crate（rustc OOM），**构建走 GitHub Actions**：

`.github/workflows/build-windows-gateway.yml`

- `windows-latest` + `npx tauri build --ci`
- 产物作为 artifact 上传
- 触发分支：`gateway` / `v152`

> 不要用 `--profile ci` 构建 exe：它带 `--cfg dev`，运行时前端会去连 `localhost:1420` 报 `ERR_CONNECTION_REFUSED`。

本地只做校验：

```bash
npx tsc --noEmit      # 前端类型
npx vite build        # 前端打包
```

---

## 9. 排错

| 现象 | 原因 / 处理 |
|---|---|
推送卡死 / `index-pack failed` | 仓库里有大文件被跟踪。用 `git rev-list --objects HEAD --not <base>` 量体积，`git rm --cached` 后加 `.gitignore` |
运行时白屏 + `ERR_CONNECTION_REFUSED localhost:1420` | 用了 `--profile ci`，改用 `npx tauri build --ci` |
网关点进去显示仪表盘 | `'gateway'` 不在 `RENDERABLE_PAGE_VALUES`，缺了会被副作用打回 `dashboard` |
保存供应商后网关卡死无响应 | `save()` 内部会再 `lock` 一次 config，必须先把锁作用域收紧 |
页面上按钮点了没反应 | 项目**没有启用 Tailwind**（无 `@tailwind` 指令 / 无配置），工具类全是死代码，只能用纯 CSS |
腾讯上游返回 `11101` | 接口只收流式，见 6.2 |
| 全部 502 `error sending request for url (...)` | 出站网络不通。先用 `curl -m 10 -o NUL -w "%{http_code}" https://www.baidu.com` 和 `curl -x http://127.0.0.1:10808 ...` 对比；若直连为 `000`、走代理正常，说明整机必须走代理，给 `proxy` 填代理地址 |
| 系统代理开关指向一个已失效的端口 | v2rayN/xray 挂了但 WinINET 代理仍开着，应用会撞死在死代理上。检查 `Get-Process v2rayN,xray` 与 `Get-NetTCPConnection -LocalPort 10808` |
| `/v1/models` 返回一堆用不了的模型 | 旧版硬编码。新版按号池平台动态生成 |
号池全是「额度未知」 | `quota_raw` 为空，去对应平台页点「刷新全部」 |
`least_used` 专挑额度最低的号 | 排序键写反了，应为 `(-quota, used)` |

---

## 10. 关键文件

| 文件 | 行数 | 作用 |
|---|---|---|
| `src-tauri/src/modules/unified_gateway.rs` | 1573 | 网关核心：号池、选路、转发、额度、日志、密钥、供应商 |
| `src-tauri/src/commands/gateway.rs` | — | Tauri 命令：`gateway_start` / `gateway_status` / `gateway_base_url` |
| `src/pages/GatewayPage.tsx` | 699 | 网关界面 |
| `src/pages/GatewayPage.css` | 322 | 纯 CSS 样式（复用项目设计变量） |
| `src-tauri/src/lib.rs` | +11 | 注册命令 + 开机自启线程 |
| `src/App.tsx` | +7 | 路由挂载 + `RENDERABLE_PAGE_VALUES` |
| `src/types/navigation.ts` | +1 | `Page` 联合类型加 `'gateway'` |
| `src/components/layout/SideNav.tsx` | +70 | 侧边栏入口 |
| `src/styles/layout.css` | +21 | 侧边栏滚动条 |

---

## 11. 已知限制

- 号池缓存 2 秒 TTL，新增账号最多 2 秒后出现
- 改端口需重启应用（保存时会返回 `restartRequired`）
- 腾讯系非流式请求由网关聚合 SSE 实现，首字延迟等于整段生成耗时
- 除 Codex / Claude / 腾讯系外，其余平台上游协议未验证
- 手动供应商在 `least_used` 下额度按 100 计算，会与满额账号竞争
