# tty7-plus @VERSION@

GitHub Actions build of tty7-plus, a fork of [tty7](https://github.com/l0ng-ai/tty7).
This is a prerelease because the macOS packages are adhoc signed and the Windows
and Linux packages are unsigned. The Android package is a signed release build.

## 下载

| 文件 | 平台 | 说明 |
|---|---|---|
| `tty7-@VERSION@-macos-arm64.dmg` | macOS Apple Silicon | adhoc 签名，未公证。首次打开需右键 → 打开，或移除 quarantine 属性。 |
| `tty7-@VERSION@-macos-x86_64.dmg` | macOS Intel | 同上。 |
| `tty7-@VERSION@-windows-x86_64-setup.exe` | Windows x86-64 | 未签名安装包。 |
| `tty7-@VERSION@-windows-x86_64.zip` | Windows x86-64 | 未签名免安装包。 |
| `tty7-@VERSION@-linux-x86_64.AppImage` | Linux x86-64 | 未签名，已打包 X11/Wayland 运行库。 |
| `tty7-@VERSION@-linux-x86_64.tar.gz` | Linux x86-64 | 未签名，依赖系统运行库。 |
| `tty7-plus-@ANDROID_VERSION@-android-release.apk` | Android 8.0+ | release 构建，使用 tty7-plus 发布密钥签名（SHA-256：`@ANDROID_CERT@`）。 |
| `tty7-relay-@VERSION@-linux-x86_64-musl` | Linux x86-64 | 静态链接的 relay，可直接部署到服务器。 |
| `tty7-relay-@VERSION@-linux-aarch64-musl` | Linux arm64 | 静态链接的 relay。 |
| `tty7-relay-@VERSION@-macos-arm64` | macOS Apple Silicon | relay 开发/测试用二进制。 |
| `tty7-relay-@VERSION@-macos-x86_64` | macOS Intel | relay 开发/测试用二进制。 |
| `tty7-relay-@VERSION@-windows-x86_64.exe` | Windows x86-64 | relay 开发/测试用二进制。 |
| `SHA256SUMS.txt` | — | 所有资产的 SHA-256。 |

## Relay 部署

Linux 静态二进制是推荐的部署形式：

```bash
sudo install -Dm755 tty7-relay-@VERSION@-linux-x86_64-musl /opt/tty7-relay/tty7-relay
sudo install -Dm600 /path/to/fullchain.pem /opt/tty7-relay/tls.crt
sudo install -Dm600 /path/to/privkey.pem   /opt/tty7-relay/tls.key
sudo install -Dm644 scripts/relay/tty7-relay.service /etc/systemd/system/tty7-relay.service
sudo systemctl daemon-reload
sudo systemctl enable --now tty7-relay.service
```

`scripts/relay/tty7-relay.service` 位于源码仓库中。详细步骤见
[`docs/remote/workspaces.mdx`](https://github.com/monsoon1360/tty7-plus/blob/main/docs/remote/workspaces.mdx#build-and-deploy)。

## 主要功能

- Android 原生远程终端：扫码配对、桌面验证码批准、自动重连、会话/工作区管理。
- 终端状态同步：当前屏幕快照 + revision 行补丁，而不是历史 ANSI 回放。
- 手机尺寸 PTY、两行按键栏、全屏程序滚动、浅色/深色主题、Agent 状态与图标。
- 自建 `tty7-relay`：仅 TLS、证书固定、端到端加密、`/healthz`、systemd 单元。
- 桌面区域快捷键 `Ctrl+1..4`、Helix/Vim 外部编辑器、全局文档布局、文件树 dotfile 记忆。
- 上游自动更新已禁用。请通过重新构建本仓库更新。

## 安全说明

- 仓库与安装包不内置任何 relay 地址、证书或指纹。默认地址
  `wss://relay.example.invalid:8443/v1/ws` 是占位域名，不会解析。
- 请在 **设置 → 配对设备** 中填写你自己的 relay 地址与叶证书 SHA-256 指纹。
- 配对二维码、设备凭据、证书与私钥不要提交到仓库。

## 校验

```sh
shasum -a 256 -c SHA256SUMS.txt
```

## 已知限制

- macOS 包未使用 Developer ID 签名，也未公证；Gatekeeper 会拦截。
- Windows 与 Linux 包未签名。
- 本 release 由 GitHub Actions 构建；如果某个平台构建失败，该平台资产会缺失，请查看 Actions 日志。
