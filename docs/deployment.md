# 一笔：部署到已有服务的云服务器

## 部署形态

一个 Rust 可执行文件、一份 SQLite 数据库、已有的 HTTPS 反向代理。前端已嵌入可执行文件，不需要安装 Node.js。默认只绑定 `127.0.0.1:3080`，公网仅经域名的 HTTPS 入口访问。

仓库提供 systemd、Nginx 和 Caddy 示例。只选择与服务器现有环境一致的反向代理。当前尚未连接或修改阿里云服务器；真实域名、证书位置、系统版本和端口占用需要部署时确认。

## 1. 构建

在开发机或 CI 上构建，避免与 2GB 服务器上的其他服务争用编译资源：

```sh
cargo test --locked
cargo build --release --locked -j 1
```

产物位于 `target/release/tally`。**构建环境必须匹配服务器的 CPU 架构与 libc 兼容性**。本项目目前在 Arch Linux / WSL 的 x86_64 环境验证；该环境生成的 GNU/Linux 二进制不能直接假定兼容旧版 Ubuntu、Alibaba Cloud Linux 等系统。实际部署应在匹配服务器的发行版容器或 CI 环境构建，或另外构建并验证 musl 目标。

## 2. 初始化目录和账户

以下命令在目标服务器执行；先确认没有同名服务账户、目录和 3080 端口冲突：

```sh
sudo useradd --system --home-dir /var/lib/tally --shell /usr/sbin/nologin tally
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

## 当前边界

- 支持 CNY 自然月分摊，不支持按天折算、多币种或多人共享。
- 导入器已验证典型 CSV/GBK/XLSX 格式；平台改版后的导出字段需要结合真实样本再确认。
- 原付款“已退款”的文字状态不足以确认具体退款金额，导入时会提醒核对；退款抵扣到账月份，不自动撤销原会员计划。
- 账本结余是期间收支差额，不等于银行余额；没有期初余额或银行卡自动同步。
- 分类使用本地规则，不调用第三方 AI 或发送账单内容到其他服务。
