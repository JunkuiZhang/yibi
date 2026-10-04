# 一笔：部署到已有服务的云服务器

## 部署形态

一个 Rust 可执行文件、一份 SQLite 数据库、已有的 HTTPS 反向代理。前端已嵌入可执行文件，不需要安装 Node.js。默认只绑定 `127.0.0.1:3080`，公网仅经域名的 HTTPS 入口访问。

仓库提供 systemd、Nginx 和 Caddy 示例。只选择与服务器现有环境一致的反向代理。当前尚未连接或修改阿里云服务器；真实域名、证书位置和端口占用需要部署时确认。

已确认的目标服务器：Ubuntu 22.04.5 LTS、x86_64、Nginx 1.18.0（Ubuntu）、Certbot 1.21.0。实际域名为 `yibi.example.com`；首次部署章节仍使用示例域名，须替换为实际域名。

## 1. 构建

在开发机或 CI 上构建，避免与 2GB 服务器上的其他服务争用编译资源：

```sh
cargo test --locked
cargo build --release --locked -j 1
```

产物位于 `target/release/tally`。**构建环境必须匹配服务器的 CPU 架构与 libc 兼容性**。本项目目前在 Arch Linux / WSL 的 x86_64 环境验证；该环境生成的 GNU/Linux 二进制不能直接假定兼容旧版 Ubuntu、Alibaba Cloud Linux 等系统。实际部署应在匹配服务器的发行版容器或 CI 环境构建，或另外构建并验证 musl 目标。

针对上述 Ubuntu x86_64 服务器，使用静态 musl 版本可避免依赖开发机的 glibc。以下命令在开发机执行，需要已安装 `musl-gcc`：

```sh
rustup target add x86_64-unknown-linux-musl
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --locked --target x86_64-unknown-linux-musl -j 1
TALLY_TEST_BINARY=target/x86_64-unknown-linux-musl/release/tally python3 scripts/smoke.py
```

产物为 `target/x86_64-unknown-linux-musl/release/tally`。发布包只需包含该文件、`deploy/`、本部署说明和校验和；不要上传本地演示数据库或账户文件。在服务器解压后先运行 `sha256sum -c SHA256SUMS` 和 `./tally --help`，再安装。

0.5.0 musl 产物在开发机验收包含 112 项基础 HTTP 检查（含 XLSX、退款和数据库备份）、101 项零元流水检查，以及混合用途分类浏览器流程，并确认是没有动态加载器或共享库依赖的 static PIE。尚未在目标服务器执行；上传后仍需完成启动和 HTTPS 验收。

## 2. 初始化目录和账户

以下命令在目标服务器执行；先确认没有同名服务账户、目录和 3080 端口冲突：

```sh
sudo useradd --system --user-group --home-dir /var/lib/tally --shell /usr/sbin/nologin tally
sudo install -d -m 0755 /opt/tally
sudo install -d -o tally -g tally -m 0700 /var/lib/tally
sudo install -m 0755 ./tally /opt/tally/tally
sudo -u tally env TALLY_DB=/var/lib/tally/tally.sqlite3 /opt/tally/tally init 你的用户名
```

最后一步在终端交互输入密码，不要把正式密码放进仓库、命令参数或部署文档。没有默认账户；无需也不提供公开注册。

## 3. 启动服务

```sh
sudo install -m 0644 deploy/tally.service /etc/systemd/system/tally.service
sudo systemctl daemon-reload
sudo systemctl enable --now tally
sudo systemctl status tally
curl --fail http://127.0.0.1:3080/health
```

单元默认使用 `MemoryHigh=192M`、`MemoryMax=256M`、最多 32 个系统任务；这是与其他服务共存的保护限制，应结合实际导入规模观察。服务自身限制为 2 个异步工作线程和最多 4 个阻塞线程。超出内存限制可能被 systemd 终止并重启，因此仍需保持备份。

如端口已占用，同时修改单元中的 `TALLY_BIND` 和反向代理的上游端口。日志：`journalctl -u tally`。运行时不记录密码、账单正文或会话令牌。

## 4. 接入域名

将账本子域名解析到服务器，接入现有的 Nginx 或 Caddy。示例文件见 `deploy/`。线上启动命令为 `tally serve`，不要加 `--dev`：生产会话 Cookie 需要 HTTPS。

Nginx 示例要求已有正确的证书；替换占位域名和证书路径后，先执行 `nginx -t`，通过后再 reload。Caddy 示例也应先按现有部署方式校验配置后 reload。不要覆盖服务器上其他服务的虚拟主机配置。

### 已有 Nginx，使用 Certbot 首次申请证书

以下流程适用于 Nginx 直接运行在宿主机、服务由 systemd 管理，且已经安装 Certbot 的 Nginx 插件。若 Nginx 由 Docker、宝塔或 1Panel 管理，先确认它实际使用的配置目录和网络；容器中的 `127.0.0.1` 不一定能访问宿主机的一笔服务。

1. 使用独立子域名，例如 `yibi.example.com`，把 DNS 的 A 记录指向服务器公网 IPv4。示例只监听 IPv4；若域名还有 AAAA 记录，须同时正确配置 IPv6 地址、监听和防火墙。当前应用使用根路径 `/`，不支持直接挂在已有网站的 `/yibi/` 子路径。
2. 确认阿里云安全组和主机防火墙允许公网访问 80、443；一笔仍只监听 `127.0.0.1:3080`，不需要开放公网 3080。第 3 节的健康检查应已通过。
3. 先备份现有 Nginx 配置，再把 `deploy/nginx-http.conf.example` 作为一个新站点加载，并替换其中的域名。它不引用尚未申请的证书。加载位置以现有 Nginx 的 `include` 为准：常见为 `/etc/nginx/conf.d/yibi.conf`，或 `sites-available` 配合 `sites-enabled` 的链接；不要把文件放进未被加载的目录，也不要为同一域名同时启用两份模板。
4. 校验并平滑重载，然后申请证书：

```sh
sudo nginx -t && sudo systemctl reload nginx
sudo certbot --nginx -d yibi.example.com --redirect
sudo nginx -t
curl --fail https://yibi.example.com/health
```

把命令中的 `yibi.example.com` 全部替换为自己的域名。Certbot 会为指定站点配置 HTTPS 和 HTTP 跳转；不要重新复制 HTTP 模板覆盖它生成的配置。Nginx 采用自定义 `-c` 配置启动时，检查命令和 Certbot 也必须指向同一套配置。

生产服务始终使用 `tally serve`。HTTP 阶段只用于连通性检查和证书签发，不要在 HTTP 页面输入账本密码；HTTPS 成功后再登录。模板设置 `client_max_body_size 9m` 以容纳 8MiB 账单和上传封装，并关闭该站点的代理缓存。

检查现有 Certbot 的自动续期任务，不要重复建立多个任务；可以针对这个证书验证续期。证书名以第一条命令的实际输出为准，未必与域名完全相同：

```sh
sudo certbot certificates
sudo certbot renew --cert-name yibi.example.com --dry-run
```

流程依据：[Nginx 反向代理](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_pass)、[Nginx 配置检查与重载](https://nginx.org/en/docs/switches.html)、[Certbot Nginx 插件](https://eff-certbot.readthedocs.io/en/stable/using.html#nginx)、[Certbot 自动续期](https://eff-certbot.readthedocs.io/en/stable/using.html#automated-renewals)。本地未安装 Nginx/Certbot，模板还需在实际服务器上执行上述配置检查。

### 部署验收

验收清单：

- HTTPS 下能登录，HTTP 会跳转 HTTPS。
- 未登录访问 `/api/entries` 和 `/api/export` 返回 401。
- 登出后旧会话不能访问数据；重启服务后数据库仍在。
- 手工补录 148 元支出，2027-02 开始 12 期：付款月份现金支出为 148 元；分摊至 2028-01，总和恰好 148 元。
- 用实际脱敏账单检查解析，重复导入时不增加同一笔交易。
- 服务器公网不开放 3080 端口，其他原有服务仍正常。

## 5. 备份与恢复

使用 SQLite 在线备份 API，而不是在 WAL 模式运行时只复制主数据库文件：

```sh
sudo install -d -o tally -g tally -m 0700 /var/lib/tally/backups
sudo -u tally env TALLY_DB=/var/lib/tally/tally.sqlite3 \
  /opt/tally/tally backup /var/lib/tally/backups/tally-2026-10-04.sqlite3
```

备份目标必须是新文件名，程序不会覆盖既有文件。完整数据库备份包含私人流水、密码哈希和会话；以敏感文件保存，并另外保留一份服务器外的备份。页面中的 JSON 导出不包含身份凭据，但第一版没有 JSON 恢复入口。

恢复时先停止 `tally` 服务，将现有主库及同名 `-wal`、`-shm` 文件移到单独的恢复前目录，保留回退能力；把备份复制到配置的 `TALLY_DB` 路径并设置属主 `tally:tally` 和权限 `0600`。然后运行一次 `tally init 你的用户名` 设置密码并撤销备份中的旧会话，最后启动服务。不要在运行中的服务下替换数据库文件。

首次部署后应执行一次备份恢复演练，并确认金额、分摊计划和规则均完整。仓库中的 HTTP 冒烟测试已经验证在线备份的 SQLite 完整性与记录数量。

## 6. 从 0.1 / 0.2 / 0.3 / 0.4 更新到 0.5.0（已有 Nginx/账户/账本）

0.5 增加跨月份的批量分类页面，支持按交易对象筛选、跨页勾选、固定单笔分类和商品关键词规则；设置中可以编辑已有规则。旧规则保持原有条件，旧账单分类不在启动时自动改变。旧记录默认未固定；批量分类默认固定，规则保存、删除和重新应用都保留固定记录。删除规则仍按剩余自定义规则重算未固定的匹配账单，未命中则待分类。数据库自动迁移到 v3，保留账户、流水、规则 ID、分摊和退款关联。无需运行 `init`，无需改 Nginx 或重新申请证书。

将新包 `yibi-linux-x86_64-0.5.0.tar.gz` 上传到服务器 `/root/yibi/`。以下按当前 root 登录执行；先解包并核验：

```sh
cd /root/yibi
tar -xzf yibi-linux-x86_64-0.5.0.tar.gz
cd yibi-linux-x86_64-0.5.0
sha256sum -c SHA256SUMS
./tally --help
```

核验成功后，在上述目录执行下面整个代码块。它只停止一笔服务，用当前安装的旧程序备份升级前数据库，再替换程序；任一步失败就停止执行，先检查错误再继续。

```sh
(
  set -eu
  YIBI_UPDATE_ID=$(date +%Y%m%d-%H%M%S)
  install -d -o tally -g tally -m 0700 /var/lib/tally/backups
  systemctl stop tally
  sudo -u tally env TALLY_DB=/var/lib/tally/tally.sqlite3 \
    /opt/tally/tally backup "/var/lib/tally/backups/before-0.5-${YIBI_UPDATE_ID}.sqlite3"
  cp -p /opt/tally/tally "/opt/tally/tally.before-${YIBI_UPDATE_ID}"
  install -m 0755 ./tally /opt/tally/tally
  systemctl start tally
  systemctl status tally --no-pager
  curl --fail http://127.0.0.1:3080/health
  curl --fail https://yibi.example.com/health
)
```

若启动失败，执行 `journalctl -u tally -n 50 --no-pager` 查看原因。**0.1–0.4 程序都不能打开 v3 数据库**；回退到旧版时须停止服务，同时恢复本次保存的旧程序与 `before-0.5-…sqlite3` 备份。按上一节“备份与恢复”将当前主库及 `-wal`、`-shm` 移到保留目录后再恢复，避免混用。回退数据会撤销升级后的新增记录和修改。

更新后刷新浏览器：

- 从流水或设置进入“整理分类”，点击京东等交易对象，按实际用途分别勾选记录并批量保存。只有订单编号时不能自动判断外卖或购物。
- “固定此笔分类”保护手工确认的例外；想重新跟随规则时，编辑单笔或批量取消固定，再在设置重新应用规则。
- 原有分类规则和分类保留。新增或编辑规则时可同时限定商户和商品关键词；删除规则会重算旧匹配范围，固定记录保留。
- 京东旧记录只有订单编号时，可手动补充商品说明。账单来源与交易号保持不变，重复导入不会新增原付款。

可再导入支付宝原账单。如果旧版已导入 22 笔非零记录，新版会跳过它们并补入 118 笔零元记录；整份再次导入不会增加流水或骑行次数。每个月的零元共享单车合并显示，付费骑行和骑行卡费用独立显示。

## 当前边界

- 支持 CNY 自然月分摊，不支持按天折算、多币种或多人共享。
- 导入器已验证典型 CSV/GBK/XLSX 格式；平台改版后的导出字段需要结合真实样本再确认。
- 原付款明确标注“已全额退款”或累计退款金额时自动抵扣原支出；其他退款状态提醒核对。独立退款仅留档，可手动关联，跨月仍回冲原支出月份；原会员按净额重新分摊，期限不变。
- 账本结余是期间收支差额，不等于银行余额；没有期初余额或银行卡自动同步。
- 分类使用本地规则，不调用第三方 AI 或发送账单内容到其他服务。
