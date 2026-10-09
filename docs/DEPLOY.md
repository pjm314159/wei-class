# 部署（Docker Compose + nginx）

> 本文档面向 **Docker Compose + nginx 的公网部署**。个人本地使用（免 nginx）直接下载
> [Releases](https://github.com/pjm314159/wei-class/releases) 预编译二进制运行即可，
> 步骤见 [README 本地部署](../README.md#本地部署)。
>
> 对应 `docs/DESIGN.md` §8：双容器（nginx 公网入口 + app 仅内网），证书与配置 volume 挂载，零持久化。

## 拓扑

```
用户 --443 HTTPS/WSS--> nginx:stable-alpine --app:8080--> app（musl 静态二进制 / distroless）
                                                             └--出站 WSS--> 微助教 /faye
```

app 不映射主机端口，公网只暴露 nginx 443；出站 WSS 由 app 容器直接发起，nginx 不参与。

## 前置检查

| 项 | 位置 | 说明 |
|---|---|---|
| TLS 证书 | `./certs/fullchain.pem`、`./certs/privkey.pem` | 以只读 volume 挂入 nginx；**不进镜像**，续期只需替换文件 |
| nginx 配置 | `./nginx.conf` | 以只读 volume 挂入；含 WS `Upgrade` 透传与 `proxy_read_timeout 75s`（须 > `WS_PING_MS`） |
| 环境变量 | `./.env`（可选） | 缺省按内置默认值运行，模板见 `.env.example` |
| 日志目录 | 容器内 `LOG_DIR`（默认 `logs`） | 按日滚动写入；如需持久化可在 compose 的 app 服务挂载 volume（如 `./logs:/app/logs`） |
| 出站网络 | 容器可访问 `www.teachermate.com.cn`、`v18.teachermate.cn` | faye 与 API 出站地址 |

`docker compose config` 可先行校验编排文件。

## 构建与启动

```bash
cp .env.example .env          # 按需调整（无 .env 亦可启动）
docker compose build          # 首次较慢：cargo-chef 依赖层在源码变动时命中缓存
docker compose up -d
docker compose logs -f app
```

## 冒烟步骤

1. `curl -I https://<域名>/` → 200 且 `content-type: text/html`
2. 浏览器打开 `https://<域名>/`，按 [README 前端冒烟清单](../README.md#前端冒烟清单) 走一遍：
   登录 → 收码 → 换轮 → 关闭 → 改间隔 → openid 失效回到登录态
3. 观察 `docker compose logs app`：出现 `服务监听中`、`faye 客户端已启动（首条活跃连接）`
4. 断开全部浏览器连接后，日志出现 `活跃连接归零，已请求 faye 客户端关闭`

## 运维

- 证书续期：替换 `./certs/` 文件后执行 `docker compose exec nginx nginx -s reload`（无需重建镜像）
- 升级：`docker compose build && docker compose up -d`（零持久化，无数据迁移）
- 查看状态：`docker compose ps`、`docker compose logs --tail=100 nginx`

## 已知约束

- 前端依赖已本地化（`static/vendor/`，服务端路由直出），无 CDN 依赖；升级依赖版本需重新下载 vendor 文件
- 镜像基于 `gcr.io/distroless/cc-debian12`（无 shell）：排查依赖日志与 `docker compose logs`；faye 出站 TLS 使用内置根证书（rustls + webpki-roots），无需挂载系统 CA
