# DeployHook

一个简单的生产部署 Webhook 服务：收到请求后验证签名，使用 `deploy` 用户执行指定更新脚本，记录执行日志。

```text
CI 推送业务镜像 → HTTPS Webhook → DeployHook → 自定义更新脚本 → Docker Compose
```

程序由 GitHub Actions 构建并发布，服务器直接下载 Release，不需要 Rust、源码或安装脚本。运行目录统一为 `/data/deployhook`，更新脚本可以放在其他位置。

## 1. 发布程序

仓库提供 `.github/workflows/release.yml`，推送 **`x.y.z` 格式的标签，不带 `v`** 后自动执行测试、构建和发布 GitHub Release。

发布前将 `Cargo.toml` 的 `version` 更新为相同版本，并提交代码；例如 `version = "0.1.0"` 对应标签 `0.1.0`。由维护者执行：

```bash
git tag 0.1.0
git push origin 0.1.0
```

Actions 构建成功后自动发布两个附件：

| 附件 | 内容 |
| --- | --- |
| `deployhook-0.1.0-linux-x86_64.tar.gz` | Linux x86_64 的 musl 构建程序、systemd 配置、环境变量示例和 Nginx 示例 |
| `SHA256SUMS` | 压缩包的 SHA-256 校验值 |

目前只构建 Linux x86_64，ARM 服务器不能使用此附件。仓库保留 `Cargo.lock`，CI 测试和构建均使用 `--locked` 固定依赖版本；部署包不包含 Cargo.lock、README、源码、测试或示例脚本。

工作流需要仓库允许 GitHub Actions 运行及 `contents: write` 权限。新版本先创建草稿并上传附件，再正式发布；失败可以重新运行。重新发布同一标签时会覆盖同名附件，维护者应限制发布标签的创建和修改权限。流程使用 [GitHub CLI 的 Release 命令](https://cli.github.com/manual/gh_release_create)。

> 本项目的 Release 用于发布 DeployHook 程序。业务镜像构建成功后发送 Webhook，是你自己业务 CI 中的另一个步骤。

## 2. 下载并准备目录

目标服务器需要 Linux x86_64、systemd、Docker Compose V2，以及 Bash、GNU `timeout`、`flock`。以下以 `0.1.0` 为例，按实际 Release 版本替换文件名。

从仓库 **Releases** 页面下载压缩包到服务器，以下假设 `deployhook-0.1.0-linux-x86_64.tar.gz` 位于当前目录。直接解压到 `/data/deployhook`，不需要中间目录。

以下服务器管理命令均以 **root** 执行。第一次安装时创建 `deploy` 用户并加入 Docker 组；账户已经存在则跳过 `useradd`：

```bash
useradd --system --user-group --no-create-home --home-dir /data/deployhook/state --shell /usr/sbin/nologin deploy
usermod -aG docker deploy
install -d -o root -g deploy -m 0750 /data/deployhook
tar --no-same-owner --no-overwrite-dir -xzf deployhook-0.1.0-linux-x86_64.tar.gz -C /data/deployhook
install -d -o deploy -g deploy -m 0700 /data/deployhook/state
chmod 0755 /data/deployhook/deployhook
install -m 0600 /data/deployhook/deploy/deployhook.env.example /data/deployhook/deployhook.env
install -m 0644 /data/deployhook/deploy/deployhook.service /etc/systemd/system/deployhook.service
```

部署后目录结构：

```text
/data/deployhook/
├── deployhook          # 程序，由 root 维护
├── deployhook.env      # 配置，由 root 保管，权限 0600
├── state/              # deploy 可写：脚本日志、锁文件、Docker 登录配置等
└── deploy/             # Release 内附的配置示例

/data/myapp/update.sh   # 示例位置，换成你自己的脚本路径
```

**`deploy` 加入 Docker 组后具有宿主机高权限**，本方案不再隔离 Webhook 与本机 Docker 权限；Docker Socket 和 Docker API 均不向公网开放。参见 [Docker 官方说明](https://docs.docker.com/engine/install/linux-postinstall/)。

## 3. 配置密钥和更新脚本

生成一个随机密钥字符串：

```bash
openssl rand -hex 32
```

将输出的 **64 位十六进制字符串本身**作为密钥，不需要解码；服务器与业务 CI 使用完全相同的字符串。不要把生成命令本身填到配置里。

编辑 `/data/deployhook/deployhook.env`：

```ini
DEPLOYHOOK_LISTEN=127.0.0.1:8787
DEPLOYHOOK_SECRET=这里替换为刚生成的密钥字符串
DEPLOYHOOK_SCRIPT=/data/myapp/update.sh
DEPLOYHOOK_SCRIPT_LOG=/data/deployhook/state/update.log
RUST_LOG=info
```

| 配置 | 说明 |
| --- | --- |
| `DEPLOYHOOK_LISTEN` | 本机监听地址，默认 `127.0.0.1:8787`；HTTPS 交给 Nginx |
| `DEPLOYHOOK_SECRET` | 必填字符串，32–256 个可见 ASCII 字节，不含空白、尾部换行 |
| `DEPLOYHOOK_SCRIPT` | 必填，自定义更新脚本的绝对路径，不附带命令参数 |
| `DEPLOYHOOK_SCRIPT_LOG` | 可选，脚本输出文件的绝对路径；每次执行覆盖，留空或不设置则输出到 journald |
| `RUST_LOG` | 程序日志级别，例如 `info`、`warn` 或排查问题时的 `debug` |

示例配置中的中文占位内容必须替换，否则启动会失败。程序不会向更新脚本传递 `DEPLOYHOOK_SECRET` 环境变量。密钥保存在服务器配置和 CI 受保护变量中，不放到 URL 或仓库里。

更新脚本可以位于 `/data/myapp/update.sh`、`/srv/myapp/update.sh` 等位置，**不要求和程序放在一起**。要求：

- `deploy` 用户可以遍历其父目录、读取并执行脚本。
- 脚本有正确的 shebang，例如 `#!/usr/bin/env bash`。
- 脚本自己 `cd` 到业务目录，执行完成返回 `0`，失败返回非零。
- 同步运行到更新完成，不把更新命令放到后台；HTTP 请求体不会成为脚本参数。

项目的 `scripts/update.sh` 仅作参考，不会自动安装或覆盖你的更新脚本。它使用 `/data/deployhook/state/update.lock` 互斥、拉取镜像后更新 Compose，并等待服务健康；使用前修改其中的业务目录、Compose 文件和项目名。

若需要与手动更新互斥，在自定义脚本中保留同一把文件锁：

```bash
exec 9>/data/deployhook/state/update.lock
flock -n 9 || exit 75
```

私有仓库登录使用 `deploy` 用户和与服务相同的 HOME：

```bash
runuser -u deploy -- env HOME=/data/deployhook/state docker login registry.example.com
```

## 4. systemd 服务配置与启动

将下面的完整配置保存为 `/etc/systemd/system/deployhook.service`，与 Release 中的 [`deploy/deployhook.service`](deploy/deployhook.service) 一致：

```ini
[Unit]
Description=DeployHook
After=network.target docker.service

[Service]
User=deploy
WorkingDirectory=/data/deployhook
EnvironmentFile=/data/deployhook/deployhook.env
Environment=HOME=/data/deployhook/state
ExecStart=/data/deployhook/deployhook
Restart=on-failure
RestartSec=5s
KillMode=control-group
TimeoutStopSec=30s
UMask=0077
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

- `WorkingDirectory` 是程序启动目录，独立于更新脚本位置。业务脚本自己切换目录即可。
- `EnvironmentFile` 由 systemd 读取，因此可以保持 root 所有、`0600` 权限。
- `Restart=on-failure` 负责程序异常退出后的重启；更新脚本失败只记日志，不重启 Webhook。
- `KillMode=control-group` 确保服务停止时清理其脚本进程。部署正在执行时不要重启服务。
- `UMask=0077` 使新建日志等文件默认仅允许运行用户访问。

服务自动继承 `deploy` 用户的组权限，加入 Docker 组后重启服务即可生效。自定义脚本、业务目录和日志目录按普通文件权限控制，确保 `deploy` 具备所需的读取、执行或写入权限。

启动与检查：

```bash
systemd-analyze verify /etc/systemd/system/deployhook.service
systemctl daemon-reload
systemctl enable --now deployhook.service
systemctl status deployhook.service
curl --fail http://127.0.0.1:8787/healthz
```

修改 `deployhook.env` 后执行 `systemctl restart deployhook.service`。修改 `.service` 文件后先 `daemon-reload` 再重启。重启会中断正在执行的更新任务，操作前先检查日志。

## 5. 日志配置

**程序日志**始终使用 journald，记录请求编号、路径、状态、耗时和脚本执行结果、退出码：

```bash
# 实时查看请求和执行结果。
journalctl -u deployhook.service -f -o cat

# 查看最近 100 条，或指定时间范围。
journalctl -u deployhook.service -n 100 --no-pager
journalctl -u deployhook.service --since '1 hour ago'
```

**更新脚本日志**由 `/data/deployhook/deployhook.env` 中的环境变量控制：

```ini
DEPLOYHOOK_SCRIPT_LOG=/data/deployhook/state/update.log
```

设置后，脚本的 stdout 和 stderr 合并写入该文件，**每次真正准备执行脚本时清空旧内容并覆盖写入**，只保留最近一次执行的输出。验签失败、重复请求、限流或正在更新时，不会清空文件。若执行进程启动失败，文件可能已被清空，对应错误仍记入 journald。

```bash
tail -F /data/deployhook/state/update.log
```

留空或删除 `DEPLOYHOOK_SCRIPT_LOG`，脚本输出会跟随程序进入 journald；设置文件后，脚本原始输出只进入文件，程序的请求日志和最终执行结果仍在 journald。`RUST_LOG` 只过滤程序日志，不过滤脚本原始输出。

日志父目录必须提前存在，并允许 `deploy` 写入。默认的 `state` 目录已满足要求。也可以自定义位置，例如：

```bash
install -d -o deploy -g deploy -m 0700 /data/myapp/logs
```

```ini
# /data/deployhook/deployhook.env
DEPLOYHOOK_SCRIPT_LOG=/data/myapp/logs/deploy.log
```

更新脚本本身只需正常输出，无需配合重定向。不要在脚本中输出凭据或开启 `set -x`。覆盖模式不会保留历史输出，也不限制单次文件大小，应控制脚本输出量；完整历史若有需要可使用 journald 模式。

更新脚本中执行镜像拉取时，建议使用普通文本进度，便于记录到文件：

```bash
docker compose --ansi never --progress plain pull
```

`--ansi never` 关闭颜色和光标控制，`--progress plain` 使用普通文本进度。拉取进度和错误输出都会进入配置的脚本日志文件；未配置文件时进入 journald。

需要重启后保留日志时，可配置 `/etc/systemd/journald.conf.d/retention.conf`，父目录不存在则先创建：

```ini
[Journal]
Storage=persistent
SystemMaxUse=512M
SystemKeepFree=1G
MaxRetentionSec=14day
```

```bash
systemctl restart systemd-journald
journalctl --flush
journalctl --disk-usage
```

这些是**整台服务器的 journald 保留策略**，会影响其他服务，不是 DeployHook 独享的日志配额，也不会清理单独的脚本日志文件。journal 保存在系统目录中；已有统一日志策略时沿用原配置即可。配置含义参见 [journald 官方说明](https://www.freedesktop.org/software/systemd/man/257/journald.conf.html)。

## 6. Nginx HTTPS 反向代理

完整示例见 [`deploy/nginx.conf`](deploy/nginx.conf)，替换域名和证书路径后放入 Nginx 的 `http` 上下文。已有站点时，只添加对应的 `location` 和所需限流配置，不必新建站点。

程序内部的触发路由固定为 `/webhook`，公网路径由 Nginx 决定。例如公网使用多级子路径：

```text
https://example.com/ops/hooks/production/deploy
                         ↓
http://127.0.0.1:8787/webhook
```

在已有 HTTPS `server` 中加入：

```nginx
location = /ops/hooks/production/deploy {
    limit_except POST { deny all; }
    client_max_body_size 16k;
    client_body_timeout 5s;
    proxy_pass http://127.0.0.1:8787/webhook;
    proxy_http_version 1.1;
    proxy_set_header Host $host;
    proxy_set_header Connection "";
    proxy_connect_timeout 2s;
    proxy_read_timeout 20s;
    proxy_send_timeout 5s;
    proxy_next_upstream off;
}
```

这里通过精确 `location` 和 `proxy_pass` 中的 `/webhook` 显式替换路径；若只写端口、不带 `/webhook`，多级原路径会传给后端并得到 404。请求体不参与路径重写，签名头和原始请求体应原样转发。路径映射规则见 [Nginx 官方说明](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_pass)。

如公网直接使用 `/webhook`，将 `location` 改为 `location = /webhook`，`proxy_pass` 保持不变。精确匹配不会接受额外的末尾 `/`，业务 CI 应填写完整地址，避免重定向。

上面的片段可以接入站点已有的限流；项目完整 `nginx.conf` 还包含请求速率与连接数限制：`limit_req_zone` / `limit_conn_zone` 放在 `http` 层，`limit_req` / `limit_conn` 放在相应服务或路径中。应用自身另有 30 秒触发冷却。默认不把 `/healthz` 代理到公网。

```bash
nginx -t
systemctl reload nginx
```

### 通过 Nginx 查看脚本日志

沿用 `/data/deployhook/state/update.log`，可以直接修改普通文件权限，不需要 ACL 或单独的日志目录。确认 `DEPLOYHOOK_SCRIPT_LOG` 已配置为此路径，日志文件生成后，由 root 执行：

```bash
chmod 0751 /data/deployhook
chmod 0711 /data/deployhook/state
chmod 0644 /data/deployhook/state/update.log
```

Nginx 既需要读取文件，也需要父目录的遍历权限，所以只修改日志文件为 `0644` 不够。该配置允许服务器上的其他本地用户读取这份日志；`state` 目录对其他用户只开放遍历，不开放目录列表。不要递归修改 `state` 权限，Docker 凭据子目录保持 `0700`，配置文件保持 `0600`。如果 `/data` 等更上级目录也有限制，需确认 Nginx 具备其遍历权限。

网页访问继续使用密码保护。以下假设 Nginx worker 用户及组为 `www-data`，如使用其他组请对应替换；`htpasswd` 工具可由发行版的软件包提供，例如 Debian/Ubuntu 的 `apache2-utils`。首次创建访问账户：

```bash
htpasswd -cB /etc/nginx/deployhook-log.htpasswd viewer
chown root:www-data /etc/nginx/deployhook-log.htpasswd
chmod 0640 /etc/nginx/deployhook-log.htpasswd
```

按提示设置一个独立的访问密码，不复用 Webhook 密钥。`-c` 仅用于首次创建密码文件，后续修改账户时去掉此参数。

在已有 HTTPS `server` 中增加单个日志文件的映射：

```nginx
location = /ops/hooks/production/log {
    alias /data/deployhook/state/update.log;

    types {}
    default_type text/plain;
    charset utf-8;

    auth_basic "DeployHook logs";
    auth_basic_user_file /etc/nginx/deployhook-log.htpasswd;

    limit_except GET { deny all; }

    open_file_cache off;
    add_header Cache-Control "no-store" always;
    add_header X-Content-Type-Options "nosniff" always;
}
```

只映射这个文件，不要把整个 `state` 目录作为网站目录。密码认证配置参见 [Nginx 官方说明](https://nginx.org/en/docs/http/ngx_http_auth_basic_module.html)。检查并加载配置：

```bash
nginx -t
systemctl reload nginx
```

访问 `https://example.com/ops/hooks/production/log`，输入账户 `viewer` 和设置的密码。刷新页面可查看当前输出，不会像 `tail -f` 一样持续推送。

程序覆盖已有日志会保留文件权限；如果删除日志后由程序重新创建，受服务 `UMask=0077` 影响，文件会变为 `0600`，需要重新执行 `chmod 0644 /data/deployhook/state/update.log`。

## 7. 业务 CI 发送 Webhook

在**业务镜像构建并推送成功后**发送请求。先将本项目的 `scripts/send-webhook.py` 复制到业务仓库的同名位置；它只需要 Python 3 标准库，不需要安装依赖。

### 设置环境变量

| 环境变量 | 是否必填 | 示例与用途 |
| --- | --- | --- |
| `DEPLOYHOOK_URL` | 是 | `https://example.com/ops/hooks/production/deploy`，填写经过 Nginx 的完整 HTTPS 地址 |
| `DEPLOYHOOK_SECRET` | 是 | 和服务器 `/data/deployhook/deployhook.env` 完全相同的密钥字符串 |
| `DEPLOYHOOK_BODY` | 否 | JSON 请求体，默认 `{}`；UTF-8 编码后最多 16 KiB |

在普通 CI 环境中，地址可作为普通环境变量配置，密钥使用 CI 的受保护变量注入。下面也可以用于手动验证，密钥占位内容需自行替换：

```bash
export DEPLOYHOOK_URL='https://example.com/ops/hooks/production/deploy'
export DEPLOYHOOK_SECRET='替换为与服务器相同的密钥字符串'
export DEPLOYHOOK_BODY='{"event":"deploy","service":"myapp","revision":"abc123"}'
python3 scripts/send-webhook.py
```

请求体也可以使用多行 JSON：

```bash
export DEPLOYHOOK_BODY='{
  "event": "deploy",
  "service": "myapp",
  "revision": "abc123"
}'
python3 scripts/send-webhook.py
```

发送脚本会读取这些环境变量，验证 JSON，基于**原始 UTF-8 请求体字节**计算签名，再发送完全相同的字节。不要在签名后重新格式化 JSON。服务端目前只把请求体用于验签，字段不会决定脚本路径、作为参数传给脚本或改变更新目标；不需要附加信息时发送 `{}` 即可。

### GitHub Actions 示例

在**业务仓库**的 `Settings → Secrets and variables → Actions → Secrets` 中，通过 `New repository secret` 添加以下三个 Actions Secrets：

| Secret 名称 | 值 |
| --- | --- |
| `DEPLOYHOOK_URL` | 完整的 HTTPS Webhook 地址，例如 `https://example.com/ops/hooks/production/deploy` |
| `DEPLOYHOOK_SECRET` | 与服务器使用的完全相同的密钥字符串 |
| `DEPLOYHOOK_BODY` | JSON 请求体，例如 `{"event":"deploy","service":"myapp"}`；不需要附加信息时填写 `{}` |

把以下 job 加入现有业务工作流的 `jobs` 下，将 `build-and-push` 替换为实际构建并推送镜像的 job 名称。`needs` 确保前置 job 成功后才触发部署，发送脚本需已提交到业务仓库：

```yaml
deploy:
  needs: build-and-push
  runs-on: ubuntu-24.04
  permissions:
    contents: read
  steps:
    - uses: actions/checkout@v6
      with:
        persist-credentials: false
    - name: Trigger production deployment
      env:
        DEPLOYHOOK_URL: ${{ secrets.DEPLOYHOOK_URL }}
        DEPLOYHOOK_SECRET: ${{ secrets.DEPLOYHOOK_SECRET }}
        DEPLOYHOOK_BODY: ${{ secrets.DEPLOYHOOK_BODY }}
      run: python3 scripts/send-webhook.py
```

### 请求协议与 curl 示例

发送的是携带签名的 HTTPS POST，请求示意如下，时间戳与签名必须每次重新计算：

```http
POST /ops/hooks/production/deploy HTTP/1.1
Host: example.com
Content-Type: application/json
X-Deploy-Timestamp: <当前 Unix 秒数>
X-Deploy-Signature: sha256=<本次计算的十六进制 HMAC>

{"event":"deploy","service":"myapp","revision":"abc123"}
```

签名公式为 `hex(HMAC-SHA256(secret, timestamp + "." + 原始请求体))`，密钥字符串不做十六进制解码。

也可以不使用发送脚本，在已经设置 `DEPLOYHOOK_URL`、`DEPLOYHOOK_SECRET`、`DEPLOYHOOK_BODY` 后直接签名并用 curl 发送：

```bash
export DEPLOYHOOK_TIMESTAMP="$(date +%s)"
SIGNATURE="$(python3 - <<'PY'
import hashlib
import hmac
import os
message = os.environ['DEPLOYHOOK_TIMESTAMP'].encode('ascii') + b'.' + os.environ['DEPLOYHOOK_BODY'].encode('utf-8')
print(hmac.new(os.environ['DEPLOYHOOK_SECRET'].encode('ascii'), message, hashlib.sha256).hexdigest())
PY
)"

curl --fail-with-body --connect-timeout 5 --max-time 25 \
  --request POST "$DEPLOYHOOK_URL" \
  --header 'Content-Type: application/json' \
  --header "X-Deploy-Timestamp: $DEPLOYHOOK_TIMESTAMP" \
  --header "X-Deploy-Signature: sha256=$SIGNATURE" \
  --data-binary "$DEPLOYHOOK_BODY"
```

不要开启 shell 命令跟踪，以免输出认证信息。发送脚本要求响应为 `202`，否则失败退出；curl 示例使用 HTTP 错误状态判断失败。两种方式都不自动重试或跟随重定向。`202` 仅表示开始执行，最终结果查看服务日志与脚本日志。

CI 与服务器应保持时钟同步。此协议是自定义 CI 请求协议，与代码托管平台原生 Webhook 的签名规则不同。

| HTTP 状态 | 含义 |
| --- | --- |
| 202 | 脚本执行进程已启动；实际成功与否看服务日志 |
| 401 | 缺少认证、签名错误或时间戳超出 ±300 秒 |
| 409 | 正在更新或请求重复 |
| 413 / 408 | 请求体过大、读取异常或读取超时 |
| 429 | 触发冷却或代理限流 |
| 503 | 无法打开脚本日志文件或启动执行进程 |

更新期间不会并行执行第二次更新。请求体上限 16 KiB，读取超时 5 秒，脚本最多执行 15 分钟，TERM 后再等 10 秒发送 KILL。HTTP 返回或客户端断开不会中断后台更新。

去重记录保存在内存，重启后清空；不承诺只执行一次。脚本退出码 `124` / `137` 可能表示超时。停止脚本不会撤销 Docker 已完成的操作，失败后检查业务状态再重试，不自动回滚。

## 8. 更新 DeployHook 程序

下载新版本压缩包到 `/tmp`，选在没有更新任务运行时更新。**保留原有 `deployhook.env`、状态目录和自定义脚本**，不用重复创建用户或重新生成密钥；以下示例中的版本号按实际附件修改：

```bash
systemctl stop deployhook.service
tar --no-same-owner --no-overwrite-dir -xzf /tmp/deployhook-0.1.1-linux-x86_64.tar.gz -C /data/deployhook
chmod 0755 /data/deployhook/deployhook
systemctl start deployhook.service
```

如新版本需要更新 `.service`，安装对应文件后执行 `daemon-reload`。更换密钥时同步修改服务器配置与业务 CI 变量，再重启服务。

本地开发可运行 `cargo fmt`、`cargo test`、`cargo build --release`；生产服务器直接使用 Release。真实 systemd、Nginx、Docker 和更新脚本仍需在目标 Linux 环境联调。
