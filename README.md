# Ausieve

把 Windows 上**除某个程序以外**的所有声音实时转进虚拟声卡。

*Capture all Windows system audio except one program (WASAPI process loopback) and play it into a virtual audio cable.*

典型用途：远控被控端同时开着 Discord 等语音软件时，RustDesk 采集系统声音会把对方的语音也传回去，造成回音。用本工具排除语音软件，再让 RustDesk 采集虚拟声卡，就没有回音了。效果类似 Parsec 的“排除应用程序”。

```
系统里所有程序的声音 ──(排除 Discord)──▶ Ausieve ──▶ 虚拟声卡播放端 ──▶ 虚拟声卡录音端 ──▶ RustDesk
```

## 准备

- Windows 10 2004（build 19041）或更高版本，64 位。
- 一个虚拟声卡，例如 [VB-Audio Virtual Cable](https://vb-audio.com/Cable/) 或 Virtual Audio Cable (VAC)。默认输出到 `Line 1 (Virtual Audio Cable)`，可以在菜单里改。

## 安装

从 [Releases](https://github.com/xuxian-pw/Ausieve/releases) 下载 `Ausieve-setup-<版本>.exe` 运行。默认装到当前用户，不需要管理员权限，需要开机自启动的话，把它的快捷方式放进启动文件夹（Win+R 输入 `shell:startup`）。

## 快速上手

1. 运行 Ausieve，系统托盘里会出现它的图标。
2. 单击图标 → **添加程序…** → 选中要排除的程序的 exe（例如 `Discord.exe`）。
3. 单击图标 → **输出设备** → 选虚拟声卡的**播放端**（默认已经是 `Line 1`）。
4. 在被控端的 RustDesk 设置里，把 **音频输入（Audio input）** 设为虚拟声卡的**录音端**，例如 `Line 1 (Virtual Audio Cable)` 或 `CABLE Output (VB-Audio Virtual Cable)`。

## 托盘菜单

```
 ○ 不排除
 ● Discord.exe
 ○ voice.exe（未运行）
──────────
添加程序…                ← 选一个 .exe，加入列表并立即选中
移除程序 ▸               ← 子菜单里显示完整路径
输出设备 ▸               ← 写入哪个播放设备
打开配置文件夹
──────────
退出
```

- **一次只能排除一个程序**（Windows API 的限制），所以列表是单选。点另一项马上切换，不用重启。
- **按完整路径匹配**：只排除路径完全相同的那个 exe，同名的其他程序不受影响。它连带启动的子进程也会一起排除。程序还没启动、退出或重启后都会自动跟上；没在运行时转发全部声音。
- **状态**：鼠标悬停在图标上显示当前排除的程序、它是否在运行、输出设备是否正常。输出设备找不到或被别的程序占用时，会弹出通知、图标变成警告样式，并每 2 秒自动重试。
- 只能运行一个实例。

## 设置

设置保存在 `%APPDATA%\Ausieve\config.json`，启动时自动恢复并开始采集，卸载时保留。文件损坏时会改名为 `config.json.bad`，然后用默认设置启动。环境变量 `AUSIEVE_CONFIG` 可以指定别的路径。

下面两项不在菜单里，需要时手动写进配置文件再重启程序：

| 项 | 默认 | 说明 |
|---|---|---|
| `"buffer_ms": 15` | 20 | 抖动缓冲，范围 11–200。越小延迟越低，越大越不容易断音。端到端延迟在 20ms 时约 40ms。 |
| `"silence_dither": true` | 关 | 静音时写入人耳听不到的 ±1 LSB 噪声（约 -90dBFS），而不是纯数字零，防止接收端因为长时间全零而暂停编码（RustDesk 约 8 秒）。 |

## 命令行版本

安装目录里的 `capture.exe` 功能和托盘程序相同，适合脚本使用。出错时直接退出，不会重试。

```powershell
capture.exe --list-devices
capture.exe --exclude-name Discord.exe --output "Line 1 (Virtual Audio Cable)"
```

| 参数 | 含义 |
|---|---|
| `--exclude-name NAME.exe` | 按进程名排除，自动找到同名的根进程，排除整棵进程树。每 250ms 检查一次，程序重启后自动跟上。 |
| `--exclude-path PATH` | 按 exe 完整路径排除（与托盘程序相同）。 |
| `--exclude-pid PID` | 按 PID 排除该进程树。 |
| `--output NAME` | 输出设备，默认 `Line 1 (Virtual Audio Cable)`。先精确匹配，再找唯一的子串匹配。 |
| `--buffer-ms N` | 抖动缓冲，默认 20，最小 11。 |
| `--silence-dither` | 静音时输出底噪，同配置文件里的 `silence_dither`。 |
| `--list-devices` | 列出播放设备和录音设备。 |

不带 `--exclude-*` 时转发全部系统声音。stdout 输出状态行：`TARGET <pid>` / `TARGET none` / `READY`。

## 已知限制

- 运行期间**独占**虚拟声卡的播放端，其他程序不能同时往这条线播放。这是必须的：用共享模式输出，自己的声音会被再次采集，形成回环。
- 只能排除**一棵**进程树。同名程序有多个独立的根进程时，只排除最早启动的那个。
- 系统里任何程序开始或停止播放时（例如调音量的提示音），Windows 会丢掉一小段音频（最多约 40ms），找不回来。本工具把它变成带淡入淡出的短暂静音，听起来是一次很短的音量下陷。
- 目标程序重启后，最初约 0.25 秒的声音可能漏进虚拟声卡。
- 有的安全软件（例如 ESET）在新程序第一次打开录音设备时会播放提示音，这个声音也会被转给远程。

## 从源码构建

需要 Rust stable（`x86_64-pc-windows-msvc`）和 Visual Studio Build Tools 的 C++ 组件（链接器和 Windows SDK 里的 `rc.exe`）。

```powershell
cargo build --release
```

生成 `target\release\` 下的 `Ausieve.exe`（托盘程序）、`capture.exe`（命令行版本）和 `tone.exe`（测试辅助程序）。

打包安装程序需要 [Inno Setup 6](https://jrsoftware.org/isinfo.php)。下面的脚本会重新构建（去掉 exe 里的本机路径），再生成 `target\installer\Ausieve-setup-<版本>.exe`，版本号取自 `Cargo.toml`：

```powershell
powershell -ExecutionPolicy Bypass -File installer\build.ps1
```

图标 `assets/app.ico`（正常）和 `assets/warn.ico`（出错）由 `python assets/make_icons.py`（需要 Pillow）生成，已经提交进仓库，平时构建不需要 Python。

## 更多文档

- [docs/testing.md](docs/testing.md)：自动化测试、每个用例的断言、测试用的环境变量。
- [docs/design.md](docs/design.md)：延迟构成，以及几次杂音问题的原因、修法和实测数据。
- [docs/audio-notes.md](docs/audio-notes.md)：音频管线的关键结论，修改相关代码前先读。

## License

MIT，见 [LICENSE](LICENSE)。
