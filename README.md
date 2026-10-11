# CloudKeep

基于 **Rust** 的百度网盘定时备份服务：**单个静态二进制**，可直接裸机运行，也可用 Docker 部署，全部配置通过 `.env` / 环境变量注入。

工作流程：启动 → 按 cron 触发 → 为每个源目录确保网盘目录存在（不存在则逐级创建）→ 各目录单独打包成 `tar.gz` → 上传到 `<BACKUP_REMOTE_DIR>/<目录名>/` → 清理该子目录下超期的历史备份（每个目录各保留最近 N 份）→ 清理本地临时包。

> **零运行时依赖。** 打包用纯 Rust 的 `tar` + `flate2`，上传直连百度网盘开放平台接口（`precreate` → `superfile2` 分块上传 → `create`），TLS 用 `rustls`（ring 后端），不依赖 OpenSSL，也不需要任何外部命令行工具。

## 特性一览

| | 说明 |
| --- | --- |
| 运行依赖 | **无**，单个静态二进制（约 5 MB，`strip` + LTO） |
| 打包 | 纯 Rust 流式写入 `tar.gz`，内存占用与文件大小无关 |
| 上传 | 开放平台分块上传（4 MB 分块、并发可调），支持秒传 |
| 调度 | 自实现 5 段 cron，支持 `*/n`、区间、列表、月/周名称 |
| TLS | `rustls` + ring，不依赖 OpenSSL 与系统开发库 |
| 代理 | 自动读取 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` |
| 平台 | Linux / Windows / macOS / FreeBSD 等（见「平台支持」） |
| 架构 | amd64 / arm64 |
| 容器 | 根文件系统只读、非 root、内置健康检查 |

## 平台支持

程序是纯 Rust 实现，除标准库与纯 Rust 依赖外不需要任何系统组件，因此在 Rust 支持的平台上都能编译运行。

| 平台 | 架构 | 产物 | 云端构建 | 常驻方式 |
| --- | --- | --- | --- | --- |
| Linux | amd64 / arm64 | `cloudkeep`（musl 静态） | **CNB** | systemd / Docker |
| Windows | amd64 | `cloudkeep.exe` | **CNB**（mingw 交叉编译） | 计划任务（见下） |
| macOS | amd64 / arm64 | `cloudkeep` | GitHub（需 macOS runner） | launchd |
| FreeBSD | amd64 | `cloudkeep` | GitHub（FreeBSD 虚拟机） | rc.d |

云端构建的分工不是随意选的，而是由平台能力决定的：

- **CNB 只能运行 Linux 容器**，因此 macOS 无法在其上产出 —— 交叉编译 macOS 需要 Apple SDK，任何 Linux 工具链都做不到
- **Windows 可以**：Alpine 提供 `mingw-w64-gcc`，用 `x86_64-pc-windows-gnu` 交叉编译即可
- **FreeBSD 需要自建 sysroot**，不适合塞进主流水线，放到 GitHub 的 FreeBSD 虚拟机里原生编译
- 三条链路最终都调用同一个 [scripts/publish-release.sh](scripts/publish-release.sh)，把产物发布到**同一个 CNB Release**，所以用户在一个页面就能拿到全部系统的二进制

已针对跨平台做的适配（都有回归测试）：

- **路径分隔符**：`BACKUP_SOURCE` 同时接受 `/data/nginx` 与 `D:\data\nginx`，会正确取出目录名；盘符（`C:`）与 UNC 路径（`\\server\share\x`）也能正确处理
- **工作目录**：默认跟随系统临时目录（Linux `/tmp/cloudkeep`，Windows `%TEMP%\cloudkeep`）；`WORK_DIR` 也接受 POSIX 写法
- **归档兼容**：无论在哪个平台打包，包内一律是 `/` 分隔的相对路径，权限位为 `0644`/`0755`，因此 Windows 上打的包在 Linux 解压同样正常
- **授权文件**：`$HOME/.cloudkeep/config.json`（Windows 为 `%USERPROFILE%\.cloudkeep\config.json`）
- **TLS**：`rustls` + ring，Windows/macOS 用系统证书库，Linux/FreeBSD 用系统 CA 目录

**不支持 32 位 ARM（armv7 / armv6）与 386**：Rust 官方基础镜像没有对应 manifest，`buildx` 会直接报 `no match for platform in manifest`。

### 各平台构建

任何平台都可以直接构建（需要 Rust 1.88+）：

```bash
cargo build --release --locked
```

想要「拷到哪都能跑」的静态二进制，Linux 上指定 musl 目标：

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl
```

> **macOS 与 FreeBSD 的二进制必须在对应系统上构建**：交叉编译需要 Apple SDK / FreeBSD sysroot，不属于常规做法。仓库提供了 [.github/workflows/release.yml](.github/workflows/release.yml)，在 GitHub 上打 tag 即可自动产出 Windows / macOS（Intel + Apple Silicon）/ FreeBSD / Linux 的二进制；若只托管在 CNB，则该工作流不会执行。

### 常驻运行

二进制本身是前台进程，`cloudkeep run` 会一直调度到收到停止信号。各平台的常驻配置模板放在 [packaging/](packaging)：

| 平台 | 文件 | 安装 |
| --- | --- | --- |
| Linux | [cloudkeep.service](packaging/cloudkeep.service) | 复制到 `/etc/systemd/system/`，`systemctl enable --now cloudkeep` |
| macOS | [com.cloudkeep.plist](packaging/com.cloudkeep.plist) | 复制到 `~/Library/LaunchAgents/`，`launchctl load` |
| FreeBSD | [cloudkeep.rc](packaging/cloudkeep.rc) | 复制到 `/usr/local/etc/rc.d/`，`sysrc cloudkeep_enable=YES` |
| Windows | [install-windows-task.ps1](packaging/install-windows-task.ps1) | `powershell -ExecutionPolicy Bypass -File install-windows-task.ps1` |

这些模板里的路径与用户名需要按实际情况调整。Windows 没有服务文件概念，脚本用「计划任务 + 开机自启」实现常驻，进程内部自己按 cron 调度。

## 目录结构

```
cloudkeep/
├── src/
│   ├── main.rs         # 入口：argv[0] 子命令派发（busybox 风格）+ 运行时
│   ├── cli.rs          # 子命令解析（含旧 --flag 兼容）与命令分发
│   ├── config.rs       # 读取并校验环境变量、生成备份目标
│   ├── cron.rs         # 5 段 cron 解析与「下次执行时间」计算
│   ├── scheduler.rs    # 定时调度、单实例互斥、优雅停机、心跳
│   ├── backup.rs       # 主流程：打包 → 上传 → 清理远程历史备份
│   ├── archive.rs      # tar.gz 打包与排除规则
│   ├── upload.rs       # 分块上传（MD5 → precreate → 并发分块 → create）
│   ├── api.rs          # 百度网盘开放平台接口封装
│   ├── remote.rs       # 网盘目录逐级创建
│   ├── auth.rs         # 无 TTY 授权：授权链接 / code 换 token
│   ├── token.rs        # 授权文件读写与 token 自动刷新
│   ├── http.rs         # HTTP 客户端与 rustls 加密后端安装
│   ├── healthcheck.rs  # 容器 HEALTHCHECK：心跳新鲜度检查
│   └── logger.rs       # 带时间戳的日志
├── Cargo.toml / Cargo.lock
├── Dockerfile          # 多阶段构建；另有 binary 阶段用于导出单文件
├── docker-compose.yml
├── .cnb.yml            # CNB 云端构建：多架构镜像 + 二进制 + Release 发布
├── .github/workflows/  # GitHub 多平台发布（Windows / macOS / FreeBSD / Linux）
├── packaging/          # 各平台常驻运行配置（systemd / launchd / rc.d / 计划任务）
├── scripts/            # 发布脚本（publish-release.sh）
├── .env.example        # 全部配置项说明
└── README.md
```

## 快速开始

### 方式一：直接用二进制（无需 Docker）

从 [CNB 构建产物](https://cnb.cool/Emptyfox-Network/CloudKeep) 下载对应架构的 `cloudkeep`（**musl 静态链接**，见下），或自行编译：

```bash
# 本机编译（需要 Rust 1.88+，用 rustup 安装最新稳定版即可）
cargo build --release
# 产物：target/release/cloudkeep
```

如果想要一个「拷贝到任何 Linux 都能跑」的静态二进制，请显式指定 musl 目标（CI 产出的就是这种）：

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
# 产物：target/x86_64-unknown-linux-musl/release/cloudkeep
```

> 直接 `cargo build --release` 在 Ubuntu/Debian 上会链接系统 glibc，只能在同一发行版族内运行；指定 musl 目标才是无依赖的静态单文件。

放到 `PATH` 里，或直接运行：

```bash
export BAIDU_APP_KEY=xxx BAIDU_SECRET_KEY=xxx
export BACKUP_SOURCE=/var/lib/docker/volumes
export BACKUP_REMOTE_DIR=/backup/docker

cloudkeep auth-url              # ① 浏览器打开链接授权
cloudkeep auth-code <code>      # ② 粘贴页面上的 code
cloudkeep backup --dry-run      # 只看打包结果，不上传
cloudkeep run                   # 按 cron 定时备份
```

静态二进制不依赖 glibc，可直接丢到任意 Linux（x86_64 / aarch64）上运行。唯一的外部要求是**系统 CA 证书**（绝大多数发行版默认自带，Alpine 需 `apk add ca-certificates`），否则 HTTPS 握手会失败。

> 需要在别的架构上运行，就用 CI 产出的对应架构二进制，或在对应架构机器上编译；本机交叉编译可用 [cross](https://github.com/cross-rs/cross)。

### 方式二：Docker（推荐用于长期运行）

```bash
cp .env.example .env
# 至少要改：BAIDU_APP_KEY、BAIDU_SECRET_KEY、BACKUP_HOST_DIR、BACKUP_SOURCE、BACKUP_REMOTE_DIR
```

App Key / Secret Key 在 [百度网盘开放平台](https://pan.baidu.com/union/console/applist) 创建应用后获取。

**首次授权**（只需一次，服务器 / NAS / ARM 设备都适用，不需要交互式终端）：

```bash
docker compose run --rm cloudkeep auth-url        # ① 打印授权链接
docker compose run --rm cloudkeep auth-code <code> # ② 用 code 换 token
```

也可以对**正在运行的容器**执行同样的命令：

```bash
docker exec cloudkeep auth-url
docker exec cloudkeep auth-code <code>
docker exec -e BAIDU_AUTH_CODE=<code> cloudkeep auth-code
```

授权成功后 token 写入容器内 `/home/cloudkeep/.cloudkeep/config.json`，该目录已挂载到宿主机 `./config`，因此会持久化；token 过期后由程序自动刷新，无需重复授权。

**先干跑一次**（可选但建议）：

```bash
docker compose run --rm -e DRY_RUN=true cloudkeep backup
```

**启动**：

```bash
docker compose up -d --build        # 本机构建镜像
docker compose logs -f
```

或跳过构建，直接拉取已发布的多架构镜像（按设备架构自动选择，无需登录）：

```bash
docker compose pull && docker compose up -d
```

`RUN_ON_START=true`（默认）时，容器启动后会立即备份一次，之后按 cron 执行。

**配置持久化**：`docker compose down`（不带 `-v`）、重建容器、升级镜像后都无需重新配置或授权；迁移到新机器时拷贝 `.env` 与 `config/` 即可。

## 常用命令

容器运行中（`docker exec`）；容器未运行时把 `docker exec cloudkeep` 换成 `docker compose run --rm cloudkeep`：

```bash
docker compose logs -f                    # 查看日志
docker compose ps                         # 查看运行状态与健康状态（healthy/unhealthy）
docker exec cloudkeep backup              # 手动执行一次备份
docker exec cloudkeep ensure-dir          # 按 BACKUP_REMOTE_DIR 新建网盘目录
docker exec cloudkeep auth-url            # 打印授权链接
docker exec cloudkeep auth-code <code>    # 用 code 换 token
docker exec cloudkeep healthcheck         # 手动跑一次健康检查
docker exec cloudkeep help                # 查看全部命令与用法
docker compose restart cloudkeep          # 重启调度
docker compose down                       # 停止
```

> `docker exec` 不经过镜像的 `ENTRYPOINT`，因此镜像内为每个子命令建了符号链接；二进制自身支持 busybox 风格的 `argv[0]` 派发，所以 `docker exec cloudkeep backup` 与 `cloudkeep backup` 等价。

二进制直接运行时同理，命令为 `cloudkeep <子命令>`：

| 命令 | 说明 |
| --- | --- |
| `run` | 启动调度（默认，无参数时也是它） |
| `backup [--dry-run]` | 备份一次后退出 |
| `ensure-dir` | 确保网盘目标目录存在 |
| `auth-url` | 打印授权链接（无 TTY 授权第 1 步） |
| `auth-code <code>` | 用授权码换 token（第 2 步） |
| `auth` | 显示授权指引（等同于 `auth-url`） |
| `healthcheck` | 健康检查，退出码 0/1 |
| `help` | 显示帮助 |

旧的 `--flag` 写法仍然兼容：`--once`（= backup）、`--mkdir`（= ensure-dir）、`--auth`、`--auth-url`、`--auth-code <code>`、`--healthcheck`。

## 配置项

完整清单见 [.env.example](.env.example)。

### 备份源

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `BACKUP_SOURCE` | `/data` | 容器内备份源，逗号分隔多个，**每个目录单独打包上传**；不存在的路径会跳过并告警，全部不存在则本次失败 |
| `BACKUP_HOST_DIR` | `./data` | 宿主机目录，由 compose 只读挂载到 `/data` |
| `BACKUP_EXCLUDE` | 空 | 打包时排除的路径片段，逗号分隔，如 `cache,.git,*.log`；按路径段匹配，支持 `*` 通配 |

需要备份多个宿主机目录时，在 `docker-compose.yml` 的 `volumes` 里追加挂载，并同步修改 `BACKUP_SOURCE`：

```yaml
volumes:
  - ./config:/home/cloudkeep/.cloudkeep
  - /var/lib/docker/volumes:/data/docker-volumes:ro
  - /etc/nginx:/data/nginx:ro
```

```dotenv
BACKUP_SOURCE=/data/docker-volumes,/data/nginx
```

### 网盘

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `BACKUP_REMOTE_DIR` | `/backup` | 网盘根目录，必须 `/` 开头（相对应用根目录）；每个源目录使用其下的同名子目录；不存在时自动逐级创建 |
| `BACKUP_NAME_PREFIX` | `backup` | 备份文件名前缀，同时用于识别可清理的历史备份 |
| `BACKUP_RETENTION` | `7` | 每个源目录各自保留最近 N 份，`0` 表示不清理 |
| `UPLOAD_CONCURRENCY` | `3` | 分块上传并发数，大文件可调到 `5` |
| `UPLOAD_ATTEMPTS` | `2` | 单次备份的上传尝试次数 |

备份文件命名：`backup_20261003_031500.tar.gz`（时间戳按 `CRON_TIMEZONE`）。

清理逻辑：每个目录上传成功后列出自己的子目录（如 `/backup/docker/nginx`），将前缀匹配且后缀为 `.tar.gz` 的**文件**按名称倒序排列，删除第 N 份之后的旧备份；清理失败只告警，不影响本次备份结果。

### 定时

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `CRON_SCHEDULE` | `0 3 * * *` | 5 段 cron：分 时 日 月 周 |
| `CRON_TIMEZONE` | `TZ` 或 `Asia/Shanghai` | 调度与文件命名时区（使用内置时区库，不依赖系统 tzdata） |
| `RUN_ON_START` | `true` | 启动后是否立即备份一次 |

cron 支持 `*`、列表 `a,b`、区间 `a-b`、步进 `*/n` 与 `a-b/n`，以及月/周名称（`jan`..`dec`、`mon`..`sun`）。当**日**与**周**都被限定时按标准 cron 语义取「或」。

常用表达式：

```
0 3 * * *       每天 03:00
0 */6 * * *     每 6 小时
30 2 * * 1      每周一 02:30
0 4 1 * *       每月 1 日 04:00
```

### 凭证与其他

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `BAIDU_APP_KEY` / `BAIDU_SECRET_KEY` | 空 | 开放平台应用凭证，仅授权时使用 |
| `BAIDU_ACCESS_TOKEN` / `BAIDU_REFRESH_TOKEN` | 空 | 可选，直接提供 token，优先级高于本地授权文件；过期自动刷新并在本次进程内生效并写回 `./config` |
| `WORK_DIR` | `/tmp/cloudkeep` | 临时目录（必须是绝对路径），压缩包上传成功后自动删除 |
| `DRY_RUN` | `false` | 只打包不上传 |
| `HEALTHCHECK_MAX_AGE_HOURS` | `26` | 心跳新鲜度阈值（小时），超过则容器标记为 unhealthy；建议略大于 `CRON_SCHEDULE` 间隔 |
| `TZ` | `Asia/Shanghai` | 容器时区 |
| `CLOUDKEEP_CONFIG_DIR` | `$HOME/.cloudkeep` | 可选，覆盖授权文件目录 |
| `IMAGE_REPO` | 见 `.env.example` | 部署使用的镜像地址 |

## 自定义备份路径

备份存放位置由 `BACKUP_REMOTE_DIR` 决定，路径相对应用根目录，必须以 `/` 开头，支持多级子目录：

```dotenv
BACKUP_REMOTE_DIR=/backup/docker/cloudkeep
```

每个源目录会在其下使用**同名子目录**，例如：

```
/backup/docker/cloudkeep/
├── nginx/          backup_20261003_031500.tar.gz
├── mysql/          backup_20261003_031500.tar.gz
└── docker-volumes/ backup_20261003_031500.tar.gz
```

开放平台没有 mkdir 接口、上传也不会自动创建父目录，因此 CloudKeep 在每次备份前会**逐级创建**该路径（`/backup` → `/backup/docker` → `/backup/docker/cloudkeep`），已存在的层级自动跳过；目录名非法、父目录异常等情况会在打包前直接报错中止，避免白跑一次上传。

只想先把目录建好、不执行备份：

```bash
docker compose run --rm cloudkeep ensure-dir
docker compose run --rm -e BACKUP_REMOTE_DIR=/backup/mysql cloudkeep ensure-dir
```

## 多文件夹备份

`BACKUP_SOURCE` 用逗号分隔多个目录，**每个目录单独打包、单独上传**，互不影响：

| 行为 | 说明 |
| --- | --- |
| 打包 | 每个目录一个 `tar.gz`，包内保留目录本身（解压即得 `nginx/`、`mysql/`） |
| 上传 | 各自上传到 `<BACKUP_REMOTE_DIR>/<目录名>/`，互不覆盖 |
| 保留 | `BACKUP_RETENTION` 对每个目录分别生效（各保留 N 份） |
| 重名 | 不同父目录下的同名目录（如 `/opt/a/data`、`/opt/b/data`）自动改名为 `a_data`、`b_data` 并输出告警日志 |
| 失败隔离 | 某个目录失败只记录错误，其余目录照常备份；汇总结果中有失败则退出码非 0 |
| 排除 | `BACKUP_EXCLUDE` 对每个目录都生效 |

## 镜像与云端构建

仓库已配置 CNB 云原生构建（[.cnb.yml](.cnb.yml)），每次 push 到 `main` 会自动：

1. **构建并推送多架构镜像**：用 `docker buildx` 生成 manifest list（`linux/amd64`、`linux/arm64`），`docker pull` 按设备架构自动选择：
   ```
   docker.cnb.cool/emptyfox-network/cloudkeep:latest
   ```
2. **导出各架构单文件二进制**：直接从刚推送的镜像中取出 `/usr/local/bin/cloudkeep`，不再重复编译（否则要多花一轮 QEMU 编译）。
3. **交叉编译 Windows 二进制**：在 `rust:1-alpine` 容器里装 `mingw-w64-gcc`，以 `x86_64-pc-windows-gnu` 目标编译出 `.exe`。这一步做成失败不致命 —— 环境变动时只会少一个产物，不会拖垮镜像。
4. **验证产物**：在干净的 `alpine` 容器里运行该二进制，确认它确实自包含、无需任何运行时。
5. **发布到 Release**：把全部二进制作为版本附件上传，供裸机直接下载（见下）。

macOS 与 FreeBSD 由 [.github/workflows/release.yml](.github/workflows/release.yml) 构建（CNB 无法产出），构建完同样调用发布脚本推送到**同一个 CNB Release**。

### 下载二进制

**方式一：从 Release 下载**（推荐）

在仓库的 Releases 页面下载对应系统的附件：

```
cloudkeep-linux-amd64        # Linux x86_64，musl 静态链接
cloudkeep-linux-arm64        # Linux aarch64，musl 静态链接
cloudkeep-windows-amd64.exe  # Windows x86_64（CNB 交叉编译）
cloudkeep-macos-amd64        # macOS Intel（GitHub 构建）
cloudkeep-macos-arm64        # macOS Apple Silicon（GitHub 构建）
cloudkeep-freebsd-amd64      # FreeBSD x86_64（GitHub 构建）
```

```bash
chmod +x cloudkeep-linux-amd64
./cloudkeep-linux-amd64 --version
```

> **发布 Release 需要令牌权限：`repo-release:rw`**
>
> 在仓库环境变量里设置 `RELEASE_TOKEN`（值为具备该权限的 CNB 访问令牌）。
> 只有 Git 权限的令牌会返回：
> ```
> errcode 10023  Missing required scopes: repo-release:rw
> ```
> 未配置该变量时，发布步骤会打印告警并**跳过**，不会影响镜像构建与其它步骤。
> 需要执行的操作：到 CNB 新建访问令牌时勾选 Release 的读写权限，再把它设为仓库的 `RELEASE_TOKEN`。
>
> 也可以用 [scripts/publish-release.sh](scripts/publish-release.sh) 在本地手动发布：
> ```bash
> TOKEN=<具备 repo-release:rw 的令牌> ./scripts/publish-release.sh dist
> ```

**方式二：从镜像里取出**（无需任何令牌，随时可用）

镜像里就是那个静态二进制，直接拷出来即可 —— 这也是验证「镜像与 Release 里的二进制一致」的方式：

```bash
docker create --name ck docker.cnb.cool/emptyfox-network/cloudkeep:latest
docker cp ck:/usr/local/bin/cloudkeep ./cloudkeep
docker rm ck
./cloudkeep --version
```

**方式三：本地构建**

```bash
docker buildx build --target binary --output type=local,dest=dist .
```

公共仓库，无需登录，任何环境直接拉取：

```bash
docker pull docker.cnb.cool/emptyfox-network/cloudkeep:latest
```

推送到自建镜像仓库（多架构）：

```bash
docker login <registry>
docker buildx build --platform linux/amd64,linux/arm64 \
  -t <registry>/<namespace>/cloudkeep:latest --push .
```

> 注意：`docker compose push` 推送的是当前设备架构的**单架构**镜像，用它覆盖 `latest` 会让多架构 manifest 退化为单架构，请改用上面的 buildx 命令。

### 关于架构支持

只构建 `linux/amd64` 与 `linux/arm64`。Rust 官方 `rust:1-alpine` 镜像**没有 32 位 ARM（`linux/arm/v7` / `arm/v6`）与 `linux/386` 的 manifest**，一旦写进 `--platform`，buildx 会直接以 `no match for platform in manifest` 失败。若确实需要 armv7，只能改用其它基础镜像单独构建一条流水线。

构建阶段不显式指定 `--platform`：buildx 多架构构建时每个阶段默认就按目标平台构建（`$TARGETPLATFORM` 是默认行为），因此不需要交叉编译工具链；代价是非本机架构（如 x86 机器上构建 arm64）编译较慢。只构建单架构时可用 `--platform` 指定。

国内构建可用 crates.io 镜像加速：

```bash
docker build --build-arg CARGO_MIRROR=sparse+https://rsproxy.cn/index/ -t cloudkeep .
```

## 运行特性

- **单实例互斥**：上一次备份未结束时会跳过本次触发，避免叠加。
- **多目录独立**：每个源目录独立打包、上传、清理，单个目录失败不影响其余目录，最终按成功数汇总。
- **重试**：上传失败后按 5s、10s… 递增间隔重试（次数由 `UPLOAD_ATTEMPTS` 决定）。
- **秒传**：命中开放平台秒传（`return_type=2`）时跳过全部分块上传。
- **低内存**：打包流式写入磁盘，上传每次只读 4 MB 分块，内存占用与文件大小无关。
- **优雅停机**：收到 `SIGTERM`/`SIGINT` 后停止调度，等待当前备份结束再退出（最多 10 分钟）；`init: true` 由 init 进程转发信号。
- **健康检查**：启动与每次备份后刷新心跳文件，超过 `HEALTHCHECK_MAX_AGE_HOURS` 未刷新自动标记 unhealthy。
- **容器加固**：根文件系统只读（`read_only`），可写目录仅 `/tmp`（磁盘卷）与挂载卷，以 uid 1000 非 root 运行。
- **日志**：JSON-file 驱动，单文件 10 MB、保留 3 个，自动轮转。
- **代理**：自动读取 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 等环境变量。

## 开发

```bash
cargo build                     # 调试构建
cargo test                      # 单元测试（cron 解析、打包/排除、MD5、配置、token 等）
cargo clippy --all-targets      # 静态检查
cargo build --release           # 发布构建（LTO + strip，约 5 MB）
```

`Cargo.lock` 已提交，镜像构建使用 `--locked` 保证可复现。

## 许可

本项目（CloudKeep 备份客户端）以 **MIT 协议**开源，可自由使用、修改与分发，
详见 [LICENSE](LICENSE)。

```
MIT License
Copyright (c) 2026 Emptyfox Network
```

> ⚠️ **CloudKeep-Server（授权中转服务端）是独立的闭源组件，不在本开源协议的覆盖范围内。**
>
> 本仓库只包含备份客户端。服务端为 Emptyfox Network 的专有软件，
> **保留所有权利**，未经书面授权不得使用、复制、修改或分发。
> 两者是彼此独立的 Cargo 项目，不存在子模块或工作区关系。
>
> 因此：本仓库的 MIT 协议**仅**适用于备份客户端自身；从本仓库获取代码
> 不代表获得服务端的任何授权。

本仓库构建时依赖的第三方开源组件（`reqwest`、`tokio`、`tar`、`flate2`、
`rustls` 等）各自适用其原有许可。

## 故障排查

| 现象 | 原因与处理 |
| --- | --- |
| 日志出现 `未找到 Access Token` | 未授权或授权文件丢失，执行 `cloudkeep auth-url` + `cloudkeep auth-code <code>` |
| `换取 Token 失败: Client authentication failed` | `BAIDU_APP_KEY` / `BAIDU_SECRET_KEY` 不匹配，或 code 已过期（code 只能用一次且几分钟内有效，重新走 `auth-url` 取新的） |
| `创建网盘目录失败: errno=-7` | 目录名含非法字符（`\ / : * ? " < > \|`），换一个名字 |
| `创建网盘目录失败` 但路径看着正常 | 检查 `BACKUP_REMOTE_DIR` 是否以 `/` 开头 |
| HTTPS 请求失败、证书错误 | 系统缺少 CA 证书（Alpine 装 `ca-certificates`）；走 TLS 拦截代理时需把自签根证书加入系统信任库 |
| 需要走网络代理 | 设置 `HTTPS_PROXY` / `HTTP_PROXY`（程序会自动读取）；注意大小写两种写法都可 |
| `没有可备份的目录` | `BACKUP_SOURCE` 与挂载路径不匹配，检查 compose 的 `volumes` |
| 上传失败但打包正常 | 检查 `BAIDU_APP_KEY` 对应的应用目录权限；上传失败会自动重试 `UPLOAD_ATTEMPTS` 次 |
| 上传速度慢 | 提高 `UPLOAD_CONCURRENCY`；百度网盘对大文件会分块上传，耗时属正常 |
| 网盘上找不到备份 | 应用只能访问自己的目录，`BACKUP_REMOTE_DIR` 是相对应用根目录的路径，且必须以 `/` 开头 |
| 时区不对（文件名/调度时间偏差 8 小时） | 修正 `TZ` 与 `CRON_TIMEZONE` |
| `WORK_DIR 必须是绝对路径` | `WORK_DIR` 需为绝对路径（默认 `/tmp/cloudkeep`） |
| `docker compose up` 拉取镜像失败 | 国内网络可在宿主机配置 Docker 镜像加速；也可先 `docker compose build` 使用本地构建的镜像 |
| 容器内权限报错 | 容器以 uid 1000 运行，挂载的宿主目录需对 uid 1000 可读 |
| 容器状态显示 `unhealthy` | 心跳文件超过 `HEALTHCHECK_MAX_AGE_HOURS` 未刷新：调度进程异常或定时备份长时间未触发；查看日志并核对 `CRON_SCHEDULE`/时区 |
| 收到停止信号后容器不退出 | 正在上传，属预期行为；`stop_grace_period` 为 10 分钟，可在大文件场景调大 |
| 命令拼写错误（如 `bakcup`） | 参数校验会直接报错并打印帮助，不会静默进入调度模式；用 `cloudkeep help` 查看可用命令 |
