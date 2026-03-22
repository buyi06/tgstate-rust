# tgState

基于 Telegram 的私有文件存储系统（Rust 版）。

将 Telegram 频道作为无限容量的文件存储后端，通过 Web 界面管理上传、下载和分享文件。

## 功能

- 通过 Web 界面或 API 上传文件到 Telegram 频道
- 大文件自动分块上传（>19.5MB），下载时流式拼接
- 短链接分享，支持在线预览常见格式（图片、视频、PDF、文本等）
- 图床模式，兼容 PicGo API
- Telegram Bot 自动同步频道文件变动（新增/删除）
- SSE 实时推送文件列表更新
- 密码保护 Web 界面
- 安全头（HSTS、CSP、X-Frame-Options 等）

## 环境变量

| 变量 | 必填 | 说明 |
|---|---|---|
| `BOT_TOKEN` | 是 | Telegram Bot Token，从 [@BotFather](https://t.me/BotFather) 获取 |
| `CHANNEL_NAME` | 是 | 目标频道（`@username` 或 `-100xxxxxxxxxx`） |
| `PASS_WORD` | 否 | Web 界面访问密码 |
| `PICGO_API_KEY` | 否 | PicGo 上传接口 API 密钥 |
| `BASE_URL` | 否 | 公开访问 URL，用于生成完整下载链接（默认 `http://127.0.0.1:8000`） |
| `DATA_DIR` | 否 | 数据目录路径（默认 `app/data`） |
| `LOG_LEVEL` | 否 | 日志级别（默认 `info`） |

## 快速开始

### Docker（推荐）

```bash
docker build -t tgstate .

docker run -d \
  -p 8000:8000 \
  -e BOT_TOKEN=your_bot_token \
  -e CHANNEL_NAME=@your_channel \
  -e PASS_WORD=your_password \
  -v tgstate_data:/app/data \
  tgstate
```

### Docker Compose

```yaml
services:
  tgstate:
    build: .
    ports:
      - "8000:8000"
    environment:
      BOT_TOKEN: your_bot_token
      CHANNEL_NAME: "@your_channel"
      PASS_WORD: your_password
      BASE_URL: https://your-domain.com
    volumes:
      - tgstate_data:/app/data
    restart: unless-stopped

volumes:
  tgstate_data:
```

### 本地编译

```bash
# 需要 Rust 1.75+
cargo build --release

# 创建 .env 文件
cp .env.example .env
# 编辑 .env 填入配置

# 运行
./target/release/tgstate
```

服务启动后访问 `http://127.0.0.1:8000`。

## API

### 文件操作

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/api/upload` | 上传文件（multipart/form-data，字段名 `file`） |
| `GET` | `/api/files` | 获取文件列表 |
| `DELETE` | `/api/files/:file_id` | 删除单个文件 |
| `POST` | `/api/batch_delete` | 批量删除（body: `{"file_ids": [...]}`) |
| `GET` | `/d/:short_id` | 通过短链接下载/预览文件 |
| `GET` | `/d/:file_id/:filename` | 旧版下载链接 |
| `GET` | `/api/file-updates` | SSE 实时文件更新推送 |

### 认证

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/api/auth/login` | 登录（body: `{"password": "..."}`) |
| `POST` | `/api/auth/logout` | 退出登录 |

### 配置管理

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/app-config` | 获取当前配置 |
| `POST` | `/api/app-config/save` | 保存配置（不应用） |
| `POST` | `/api/app-config/apply` | 保存并应用配置 |
| `POST` | `/api/reset-config` | 重置所有配置 |
| `POST` | `/api/set-password` | 设置密码 |
| `POST` | `/api/verify/bot` | 验证 Bot Token |
| `POST` | `/api/verify/channel` | 验证频道可用性 |

### PicGo 兼容上传

```bash
curl -X POST http://your-host:8000/api/upload \
  -H "X-Api-Key: your_picgo_api_key" \
  -F "file=@image.png"
```

## 项目结构

```
tgstate-rust/
├── src/
│   ├── main.rs              # 入口，服务器启动
│   ├── config.rs            # 配置管理
│   ├── database.rs          # SQLite 数据库操作
│   ├── error.rs             # 错误处理
│   ├── events.rs            # 事件总线（SSE 推送）
│   ├── auth.rs              # 认证工具
│   ├── state.rs             # 应用状态与 Bot 生命周期
│   ├── middleware/
│   │   ├── auth.rs          # 认证中间件
│   │   └── security_headers.rs  # 安全头中间件
│   ├── routes/
│   │   ├── api_auth.rs      # 登录/登出
│   │   ├── api_files.rs     # 文件下载/列表/删除
│   │   ├── api_settings.rs  # 配置管理
│   │   ├── api_sse.rs       # SSE 端点
│   │   ├── api_upload.rs    # 文件上传
│   │   └── pages.rs         # 页面渲染
│   └── telegram/
│       ├── bot_polling.rs   # Bot 轮询处理
│       ├── service.rs       # Telegram API 封装
│       └── types.rs         # Telegram 类型定义
├── app/
│   ├── templates/           # Tera HTML 模板
│   └── static/              # CSS/JS 静态资源
├── Cargo.toml
├── Dockerfile
└── .env.example
```

## 技术栈

| 组件 | 技术 |
|---|---|
| Web 框架 | Axum 0.7 |
| 异步运行时 | Tokio |
| 模板引擎 | Tera |
| 数据库 | SQLite (rusqlite, WAL 模式) |
| HTTP 客户端 | reqwest |
| 序列化 | serde / serde_json |

## License

MIT
