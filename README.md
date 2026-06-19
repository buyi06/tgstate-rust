# tgState

> 基于 Telegram 的私有文件存储系统 · Rust 构建 · 单二进制部署 · 开箱即用

把 Telegram 频道当作无限容量的存储后端，通过一个暗色极简的 Web 界面上传、管理、分享文件，并自带图床模式。

---

## 特性

**存储与传输**
- 网页拖拽 / 多选上传，文件直存 Telegram 频道
- 大文件（>19.5MB）自动分块，**多块并发上传**；下载时**并发预取**分块、按序流式拼接
- `UPLOAD_CONCURRENCY` 可调上传并发，权衡速度与内存
- Telegram Bot 长轮询同步频道文件变动，SSE 实时推送到前端

**分享**
- 每次上传生成短链 `/d/<short_id>`，支持在线预览（图片 / 视频 / PDF / 文本）
- **逐文件分享密码**：可为单个文件设置访问密码，访客需输入密码才能下载
- 图床模式：图片网格画廊，悬浮操作，URL / Markdown / HTML 一键批量复制

**界面**
- 暗色极简风格（Linear / Vercel 取向），亮 / 暗主题自适应，移动端适配
- 网页引导式首启配置，无需预填环境变量

**安全**
- argon2 密码哈希 + 与密码无关的随机会话令牌
- CSRF 防护（Origin 校验 + `SameSite=Strict` cookie）
- 全站输出转义、下载侧主动屏蔽可执行内容、安全响应头
- 分桶限流（登录 / 上传 / API / 下载），日志脱敏 bot token

---

## 快速开始

### 方式一：Docker（推荐）

容器内服务监听 **7860**（兼容 Hugging Face Spaces），用 `-p 主机端口:7860` 映射：

```bash
docker run -d --name tgstate -p 8000:7860 -v tgstate_data:/app/data $(docker build -q .)
```

Docker Compose：

```yaml
services:
  tgstate:
    build: .
    ports:
      - "8000:7860"
    volumes:
      - tgstate_data:/app/data
    restart: unless-stopped

volumes:
  tgstate_data:
```

### 方式二：预编译二进制

```bash
wget https://github.com/buyi06/tgstate-rust/releases/latest/download/tgstate-linux-amd64.tar.gz
tar xzf tgstate-linux-amd64.tar.gz
cd tgstate && ./tgstate
```

源码 / 二进制方式默认监听 `8000`，可用 `PORT` 覆盖。

### 方式三：从源码编译

```bash
# 需要 Rust 1.82+
cargo build --release
./target/release/tgstate
```

启动后访问 `http://你的IP:8000`，按引导设置管理员密码，再到「设置」页填入 Bot Token 与频道。

---

## 配置流程

1. 首次访问 Web 界面，设置管理员密码
2. 登录后进入「设置」
3. 填写 Bot Token（从 [@BotFather](https://t.me/BotFather) 获取）与频道（`@username` 或 `-100xxxxxxxxxx`，机器人需为该频道管理员）
4. 点击「保存并应用」

所有配置存于本地 SQLite，无需 `.env` 文件。

---

## 环境变量（全部可选）

仅用于预配置场景（如 Docker 部署跳过网页配置）。

| 变量 | 说明 | 默认值 |
|---|---|---|
| `BOT_TOKEN` | Telegram Bot Token | - |
| `CHANNEL_NAME` | 目标频道 `@name` 或 `-100xxx` | - |
| `PASS_WORD` | Web 界面管理员密码 | - |
| `BASE_URL` | 生成分享链接用的公开 URL | `http://127.0.0.1:8000` |
| `PORT` | 监听端口 | `8000`（Docker 镜像内为 `7860`） |
| `DATA_DIR` | 数据目录 | `app/data` |
| `LOG_LEVEL` | 日志级别 | `info` |
| `UPLOAD_CONCURRENCY` | 大文件分块并发上传数（1-16，越大越快越吃内存，峰值 ≈ 值 × 19.5MB） | `3` |
| `SESSION_MAX_AGE_SECS` | 会话 Cookie 有效期（秒） | `604800`（7 天） |
| `COOKIE_SECURE` | 强制 `Secure` Cookie（反代终止 TLS 时） | 自动推断 |
| `TRUST_FORWARDED_FOR` | 信任 `X-Forwarded-For` / `X-Real-IP` 识别客户端 IP | `0` |

> 反向代理部署时，建议同时设置 `COOKIE_SECURE=1` 与 `TRUST_FORWARDED_FOR=1`，否则 Cookie 可能缺 `Secure` 标志、且限流会把所有请求合并到代理 IP。`TRUST_FORWARDED_FOR` 仅在前置代理可信时开启。

---

## 工作原理

- **小文件**：单次 `sendDocument` 上传，数据库记录 `message_id:file_id`。
- **大文件**：按 ~19.5MB 切块并发上传，再写一份 `tgstate-blob` manifest（列出各块），数据库只存 manifest 的复合 ID。
- **下载**：读 manifest → 并发预取各块下载地址与连接 → 按序流式输出，单块不整体进内存。
- **同步**：Bot 长轮询 `getUpdates`，频道内新增 / 删除的文件会反映到列表（仅限配置的频道）。

---

## API

### 文件

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/api/upload` | 上传文件（multipart，字段名 `file`，需登录会话） |
| `GET` | `/api/files` | 文件列表 |
| `DELETE` | `/api/files/:file_id` | 删除文件 |
| `POST` | `/api/batch_delete` | 批量删除 |
| `POST` | `/api/files/:file_id/share-password` | 设置 / 清除分享密码（空密码 = 清除） |
| `GET` | `/d/:short_id` | 短链下载 / 预览（有密码时需先解锁） |
| `GET` | `/api/file-updates` | SSE 实时更新 |

### 分享

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/share/:short_id` | 分享页（有密码时显示密码输入页） |
| `POST` | `/share/:short_id/unlock` | 校验分享密码、写解锁 Cookie |

### 认证 / 配置

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/api/auth/login` · `/api/auth/logout` | 登录 / 退出 |
| `GET` | `/api/app-config` | 获取配置（不含明文密钥） |
| `POST` | `/api/app-config/apply` · `/save` · `/reset-config` | 保存 / 应用 / 重置 |
| `POST` | `/api/verify/bot` · `/api/verify/channel` | 验证 Bot / 频道 |
| `GET` | `/api/health` | 健康检查 |

---

## 安全

- **认证**：argon2 哈希密码；登录后下发与密码无关的随机会话令牌（存于服务端，登出即失效）。
- **分享密码**：逐文件 argon2 哈希；校验通过后下发 HttpOnly 解锁 Cookie（存哈希、不可逆）。
- **CSRF**：状态变更请求校验 `Origin` 是否同源，叠加 `SameSite=Strict` cookie。
- **XSS**：模板自动转义；前端所有动态内容经显式 HTML 转义；下载侧仅对图片 / 视频 / PDF / 文本等内联预览，对 `html` / `svg` / `js` 等强制 `attachment` 并加 `X-Content-Type-Options: nosniff`。
- **限流**：登录 / 上传 / API / 下载分桶按客户端 IP 限流，限流表满时淘汰最旧项而非拒绝新用户。
- **其它**：bot token 从错误日志中脱敏；CSP / `X-Frame-Options` / `Referrer-Policy` / `Permissions-Policy` 等安全头；容器以非 root 运行。

> 短链 `short_id` 本身即一种持有型凭据。若链接泄露且未设分享密码，建议删除文件（短链无法在保留数据的前提下轮换）。

---

## 技术栈

| 组件 | 技术 |
|---|---|
| Web 框架 | Axum 0.7 |
| 异步运行时 | Tokio |
| 模板引擎 | Tera |
| 数据库 | SQLite（WAL）+ r2d2 连接池 |
| HTTP 客户端 | reqwest |
| 密码哈希 | argon2 |
| CI / CD | GitHub Actions |

---

## 项目结构

```
├── src/
│   ├── main.rs                  # 入口、路由装配、中间件
│   ├── config.rs                # 配置（env + DB 合并）
│   ├── database.rs              # SQLite（文件元数据 / 设置 / 分享密码）
│   ├── auth.rs                  # 密码哈希、会话与分享解锁 cookie
│   ├── state.rs                 # 应用状态、Bot 生命周期
│   ├── middleware/              # 认证+CSRF / 限流 / 安全头
│   ├── routes/                  # 上传 / 文件 / 设置 / 页面 / SSE
│   └── telegram/                # Bot 轮询与 Telegram API
├── app/
│   ├── templates/               # Tera 模板（含 share_unlock.html）
│   └── static/                  # ui.css / ui.js / js/main.js
├── .github/workflows/           # CI / 发布 / GHCR
├── Dockerfile
└── Cargo.toml
```

---

## License

MIT
