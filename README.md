# CF Hosts GUI

Cloudflare 优选 IP 管理工具(Rust + egui):调用 [XIU2/CloudflareSpeedTest](https://github.com/XIU2/CloudflareSpeedTest) 测速,把最优 IP 以**区块化**方式写入 hosts,支持一键更新与定时无人值守更新。

## 功能

- **域名管理**:添加/移除要优选的 CF 域名,支持为单个域名手动固定 IP(否则自动跟第一名)
- **一键测速并更新**:cfst 测速 → 解析结果 → UAC 提权写入 hosts → 刷新 DNS,一个按钮完成
- **安全写入**:
  - 只动 `# --- cf-hosts-gui BEGIN/END ---` 标记内的自有区块,绝不碰用户其他内容
  - 区块外的同域名旧记录(如手工加的)自动注释接管,diff 全程记录
  - 写入前双备份(etc 目录 + `%APPDATA%/cf-hosts-gui/backups/`)、写入后读回校验、失败自动回滚
  - 「还原全部」一键移除管理区块
- **定时更新**:注册 Windows 计划任务(最高权限、无弹窗),定时跑 `--auto` 无头模式
- **0 条结果保护**:cfst 测速失败/空结果时保留现有 hosts 不动

## 运行模式

| 命令 | 说明 |
|------|------|
| `cf-hosts-gui.exe` | GUI 模式(普通权限,写 hosts 时弹一次 UAC) |
| `cf-hosts-gui.exe --auto` | 无头模式:测速 → 更新 hosts → 写日志 → 退出(计划任务用) |
| `cf-hosts-gui.exe --write-hosts <job.json>` | 提权 helper(GUI 内部调用,勿手动执行) |
| `cf-hosts-gui.exe --selftest` | 核心逻辑自检,不触碰系统文件 |

## 构建

```bash
cargo build --release
```

本机环境:rustup(GNU 工具链,`D:\rustup` / `D:\cargo`)+ WinLibs MinGW(`dlltool`,windows-sys 编译需要)。

> 说明:egui 标准 Windows 链路是 MSVC,本机为避免安装 VS C++ 工作负载(约 3GB)选了 GNU 链路;
> 正式发布可用 GitHub Actions 的 `windows-latest`(预装 VS 2022)云端出 msvc 版。

## 分发打包

目录结构(exe 同级):

```
cf-hosts-gui/
├── cf-hosts-gui.exe
└── bin/
    ├── cfst.exe        # 测速引擎(GPL-3.0,须随包附带其 LICENSE,见 bin/cfst-LICENSE)
    └── ip.txt          # Cloudflare 官方 IP 段
```

exe 会依次探测:`bin/cfst.exe` → exe 同目录 → `../cfst_windows_amd64/cfst.exe`(开发目录布局),也可在设置页指定绝对路径。

## 许可

- 本项目代码:另行声明(未指定前按版权所有处理)
- `bin/cfst.exe`:GPL-3.0,独立进程调用不传染本项目代码;随包分发须附其 LICENSE 与来源说明
