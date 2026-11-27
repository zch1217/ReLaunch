# ReLaunch

轻量级 Windows 桌面应用一键重启与快捷启动工具（基于 Rust + `egui` / `eframe`），**Restart any app in one click**。

解决 Electron 应用、Microsoft Store (MSIX) 应用以及托盘常驻应用进程分散、或自动更新后安装目录变化导致难以快速重启的问题。

## 主要特性

- **一键重启 / 启动**
  - 对正在运行的应用直接强制结束进程树并重新拉起；对未运行的收藏应用一键直接启动。相比向窗口发送关闭消息，直接终止可避免 Electron / 托盘常驻应用（如 Codex、Claude、ChatGPT 等）仅隐藏到托盘或保留后台进程，确保重启后完整重载配置。
  - **三项安全保障**：
    1. **终止前验证启动入口**：重启前先解析并确认启动入口存在（包括校验 MSIX AUMID 注册状态），找不到有效入口时绝不关闭原程序。
    2. **终止前核验进程身份**：打开进程句柄后再次核验可执行文件与身份匹配，防止 PID 复用导致误杀其他进程。
    3. **确认所有目标进程退出后再启动**：等待目标进程树完全退出后再拉起新实例（超时则中止并报错），避免旧进程残留导致单实例应用忽略新启动或未重载配置。
  - > **注意**：由于重启过程会直接强制终止目标进程，Word、Photoshop、代码编辑器等可能存在未保存内容的软件在重启时会丢失未保存的数据，请先保存后再执行重启。
- **完整支持 MSIX 与 Win32 应用**
  - **MSIX / Store 应用**：通过 `GetApplicationUserModelId` 或 `AppxManifest.xml` 提取 AUMID，按 Package Family 聚合全部子进程，并使用 `IApplicationActivationManager`（失败时回退 `ShellExecuteW`）激活，不受版本目录变更影响。
  - **Win32 / Electron 应用**：以可执行文件名 + PE 版本资源 `ProductName` 作为稳定身份，同一会话内按路径聚合所有子进程；每次重启实时按 `App Paths 注册表 > 开始菜单快捷方式 > 桌面快捷方式 > 最近已知路径` 顺序动态解析启动入口（兼容 Squirrel `Update.exe --processStart`），不依赖写死的旧路径。
- **收藏管理与后台托盘应用识别**
  - 收藏列表持久化保存至 `%APPDATA%\ReLaunch\config.json`（临时文件 + 重命名原子写入）。
  - 即使应用已最小化到系统托盘、没有可见窗口，仍可通过进程身份准确识别其运行状态。
- **已安装应用扫描面板**
  - 点击顶部 `+` 按钮，结合 `Get-StartApps` 与开始菜单 `.lnk` 快捷方式枚举系统已安装应用。
  - 支持扫描进度提示、实时搜索、`MSIX` / `Win32` 类型标识以及一键收藏/取消收藏。
- **系统托盘常驻与开机自启**
  - 关闭主窗口自动隐藏到系统托盘（通过托盘菜单的「退出」彻底结束程序）。
  - 左键点击托盘图标唤起主窗口，鼠标悬停刷新运行状态，右键菜单支持直接重启/启动收藏应用、切换开机自启。
  - 开机自启写入当前用户注册表，登录时以 `--minimized` 参数静默启动到托盘。
- **现代界面与实用细节**
  - 支持浅色 / 深色模式切换（自动记忆）、3 秒自动刷新、实时搜索（按 `Esc` 清空）。
  - 应用卡片展示所在目录，支持一键复制目录/程序路径或直接在资源管理器中定位文件。
  - 单实例互斥锁（`ReLaunch_SingleInstance`），重复启动时自动唤起已有窗口；内建自身进程保护，避免误操作。

## 运行环境

- **操作系统**：Windows 10 / Windows 11（x64）

## 构建与打包

### 开发调试与 Release 编译

```powershell
# 开发模式运行
cargo run

# 编译 Release 单文件可执行程序（产物位于 target\release\relaunch.exe）
cargo build --release
```

`Cargo.toml` 已配置 `opt-level = "z"`、LTO、`strip = true` 以及静态 CRT（`.cargo/config.toml`），无需额外运行时依赖。

### 打包便携版与安装程序

需提前安装 [Inno Setup 6](https://jrsoftware.org/isinfo.php)（`ISCC.exe`），然后运行打包脚本：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/package.ps1
```

脚本会自动编译 Release 版本并在 `dist/` 目录生成：
- `dist\ReLaunch.exe`：单文件便携版
- `dist\ReLaunch-Setup-0.1.0.exe`：安装包（默认安装到 `%LOCALAPPDATA%\Programs\ReLaunch`，无需管理员权限，支持可选桌面快捷方式与开机自启）

## 命令行参数

| 参数 | 说明 |
| --- | --- |
| `--minimized` / `--autostart` | 静默启动并最小化到系统托盘 |
| `--list` | 诊断模式（只读）：列出当前识别到的运行中应用、身份、PID 及启动入口 |
| `--installed` | 诊断模式（只读）：枚举并打印已安装应用列表 |
| `--restart <应用名>` | 不启动 GUI，按显示名称直接重启指定正在运行的应用 |

如需查看详细调试日志，可设置环境变量：

```powershell
$env:RUST_LOG = "debug"
cargo run
```

## 配置与权限说明

- **配置文件路径**：`%APPDATA%\ReLaunch\config.json`（保存收藏应用与深浅色主题偏好）
- **开机自启注册表项**：`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`（值名 `ReLaunch`）
- **管理员权限提示**：如果目标程序本身以管理员权限运行，ReLaunch 需要以管理员身份运行才能终止并重启它。

## 目录结构

```text
ReLaunch/
├── .cargo/config.toml      # Windows 静态 CRT 编译配置
├── assets/                 # 构建生成的多尺寸应用图标
├── build.rs                # 生成 .ico 并嵌入 Windows 版本信息与 DPI 清单
├── installer/relaunch.iss  # Inno Setup 6 安装包脚本
├── scripts/package.ps1     # 一键编译与打包脚本
└── src/
    ├── main.rs             # 程序入口、单实例保护与 CLI 诊断参数
    ├── app.rs              # 全局状态、后台刷新与任务调度
    ├── ui.rs               # 主列表、添加应用面板与状态栏界面
    ├── theme.rs            # 浅色 / 深色主题配色与矢量图标绘制
    ├── com.rs              # 线程 COM 初始化与 RAII 释放封装
    ├── discovery.rs        # 窗口/进程枚举与 MSIX / Win32 身份识别
    ├── installed.rs        # 已安装应用扫描（Get-StartApps + 开始菜单快捷方式）
    ├── identity.rs         # 应用身份与运行时信息模型
    ├── launcher.rs         # 启动入口动态解析与程序拉起
    ├── process.rs          # 进程树终止与退出等待
    ├── restart.rs          # 重启/启动流程编排与新进程校验
    ├── config.rs           # 配置文件原子读写
    ├── autostart.rs        # 注册表开机自启管理
    ├── tray.rs             # 系统托盘图标与右键菜单
    ├── icon.rs             # 程序内置品牌图标生成
    └── icon_extract.rs     # 从 EXE / 快捷方式提取应用图标
```

## 许可证

本项目基于 [MIT License](LICENSE) 开源。
