# 🖱️ ShareMouse

跨平台键鼠共享工具 —— 一套鼠标键盘在 **macOS** 与 **Windows** 之间无缝切换。
光标移到屏幕边缘即可「穿越」到另一台电脑，类似 Synergy / Barrier 的软件 KVM。

> 基于 **Rust + Tauri 2.x + React + TypeScript**，体积小（~3MB），局域网延迟极低。

## ⬇️ 下载安装

前往 [Releases](https://github.com/code4henry/share-mouse/releases) 下载最新版本：

| 平台 | 文件 |
|------|------|
| **macOS** (Apple Silicon) | `ShareMouse_0.1.0_aarch64.dmg` |
| **Windows** (x64) | `ShareMouse_0.1.0_x64-setup.exe` 或 `.msi` |

## 🚀 使用方法

1. 两台机器连接 **同一个 WiFi / 局域网**
2. 在**有物理鼠标键盘**的机器上打开 ShareMouse → 点击 **Host**
3. 在另一台机器上打开 ShareMouse → 输入 Host 的 `IP:24800` → 点击 **Connect**
4. 把光标移到屏幕边缘，它会自动「穿越」到另一台电脑

> 查 IP：Mac 上 `ifconfig | grep "inet "`，Windows 上 `ipconfig`

## ⚠️ macOS 重要：首次运行须知

由于本应用**未经 Apple 代码签名和公证**，从网上下载后打开会提示「**文件已损坏**」或「**无法验证开发者**」。这是 macOS Gatekeeper 安全机制，应用本身没有问题。

**解决方法（任选其一）：**

### 方法一：终端命令（推荐）

```bash
# 1. 下载 dmg 后，先清除 dmg 的隔离标记
xattr -cr ~/Downloads/ShareMouse_*.dmg

# 2. 双击打开 dmg，把 ShareMouse 拖到「应用程序」文件夹

# 3. 清除 app 的隔离标记（关键步骤）
xattr -cr /Applications/ShareMouse.app

# 4. 现在可以正常双击打开了
```

### 方法二：右键打开

在「访达」里**右键**点击 ShareMouse.app → **打开** → 弹窗里再点 **打开**。

---

### 授权辅助功能权限（必需）

ShareMouse 需要捕获你的键盘和鼠标输入，必须授予辅助功能权限：

**系统设置 → 隐私与安全性 → 辅助功能** → 开启 **ShareMouse**

> 不授权的话，光标穿越和键鼠转发都不会工作。

## 🛠️ 本地开发

### 环境要求

- [Rust](https://rustup.rs/) (stable)
- [Node.js](https://nodejs.org/) 18+
- macOS: Xcode Command Line Tools (`xcode-select --install`)
- Windows: Visual Studio C++ Build Tools

### 运行

```bash
npm install
npx tauri dev      # 开发模式（热重载）
npx tauri build    # 生成安装包
```

## 🏗️ 架构

```
┌─────────────────┐     TCP :24800     ┌─────────────────┐
│     macOS        │◄──────────────────►│    Windows       │
│  CGEvent API     │   二进制协议        │  Win32 API       │
│  Host / Client   │   (bincode)        │  Host / Client   │
└─────────────────┘                    └─────────────────┘
```

- **Host**：物理连接鼠标键盘的机器，捕获输入并转发
- **Client**：接收输入事件并注入到本机系统
- 两端使用相同的二进制协议，仅底层调用不同的 OS API

## 📁 项目结构

```
src-tauri/src/
├── core/
│   ├── protocol.rs    # 输入事件协议（bincode 编解码）
│   ├── network.rs     # TCP 异步通信层（tokio）
│   ├── screen.rs      # 屏幕布局 + 边缘检测
│   └── mod.rs         # 引擎（角色管理 + 事件路由）
├── platform/
│   ├── mod.rs         # 平台抽象 trait
│   ├── macos.rs       # macOS CGEvent 实现
│   └── windows.rs     # Windows Win32 实现
└── commands.rs        # Tauri IPC 命令

src/
└── App.tsx            # React 前端（暗色主题 UI）
```

## 📄 License

MIT
