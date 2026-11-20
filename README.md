# audio-exclude

**Capture all Windows system audio except one program, and feed it into a virtual audio cable** —
e.g. so a remote-desktop tool such as RustDesk can stream the host's sound without echoing the
host's voice-chat app (similar to Parsec's "exclude application" option).
Uses WASAPI *process loopback* (`PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE`).

把 Windows 上**除某个程序以外**的所有声音实时转进虚拟声卡。典型用途：远控被控端同时开着 Discord 等语音软件时，
RustDesk 采集系统声音会把语音软件的声音也传给对方，造成回音；用本工具排除语音软件，再让 RustDesk 采集虚拟声卡即可。

## 前置条件

- Windows 10 2004（build 19041）或更高版本，Process Loopback API 从这个版本开始提供。
- 一个虚拟声卡，例如 [VB-Audio Virtual Cable](https://vb-audio.com/Cable/) 或 Virtual Audio Cable (VAC)。
  它要有一对播放端/录音端，并允许独占模式（Windows 默认允许）。
- 从源码构建需要 Rust stable，工具链为 `x86_64-pc-windows-msvc`，并安装 Visual Studio Build Tools 的 C++ 组件（提供链接器）。

## 构建

```powershell
cargo build --release
```

生成 `target\release\capture.exe`（主程序）和 `target\release\tone.exe`（测试辅助程序）。

## 用法

```powershell
# 列出设备名
.\target\release\capture.exe --list-devices
# 排除 Discord，把其余声音写到虚拟声卡的播放端
.\target\release\capture.exe --exclude-name Discord.exe --output "Line 1 (Virtual Audio Cable)"
```

| 参数 | 含义 |
|---|---|
| `--exclude-name NAME.exe` | 按进程名排除。自动找到该名字的**根进程**（父进程不是同名进程的那个），排除整棵进程树，所以靠子进程出声的程序（Discord、浏览器）也能排除干净。每 250ms 检查一次，程序退出或重启后会自动重建采集；程序没在运行时转发全部声音。 |
| `--exclude-pid PID` | 按 PID 排除该进程树。 |
| `--output NAME` | 输出用的播放设备，默认 `Line 1 (Virtual Audio Cable)`。先按名字精确匹配，再找唯一的子串匹配。 |
| `--list-devices` | 列出播放设备和录音设备。 |

不带 `--exclude-*` 参数时转发全部系统声音。stdout 会输出状态行：`TARGET <pid>` / `TARGET none` / `READY`。

### 配置 RustDesk

在被控端的 RustDesk 设置里，把 **音频输入（Audio input）** 设为虚拟声卡的**录音端**，
例如 `Line 1 (Virtual Audio Cable)` 或 `CABLE Output (VB-Audio Virtual Cable)`。
capture 写虚拟声卡的播放端，虚拟声卡把声音送到对应的录音端，RustDesk 再从录音端读取。

## 测试

```powershell
cargo test -- --nocapture
```

测试在单机上用真实音频设备运行（不 mock），全部跑完约 20 秒，期间会从默认播放设备放出 -20dBFS 的测试音。
每个进程用不同频率的正弦波标记（被排除的 A 是 1000Hz，保留的 B 是 440Hz），用 Goertzel 算法测频点电平，
用 FFT 互相关测延迟。每次都同时录两路：虚拟声卡的录音端，以及测试音所在播放设备的 loopback（即本地实际听到的声音），后者作为参照。

| 用例 | 断言 |
|---|---|
| t1 按 PID 排除 | 440Hz 存在，1000Hz 比 440Hz 低 ≥40dB |
| t2 对照组 | 排除一个无关进程时，两个频率都到达虚拟声卡，电平与本地输出相差 <3dB |
| t3 无回环 | B 只播放 200ms；虚拟声卡里的声音不能比本地更响、更长，本地静音 80ms 后虚拟声卡也必须静音 |
| t4 进程重启 | 按名字排除，A 被杀掉后重新启动（PID 变化），仍然被排除 |
| t5 子进程出声 | A 本身不出声，由它启动的子进程出声；按名字排除根进程仍然有效 |
| t6 延迟 | 30ms 脉冲在本地输出与虚拟声卡之间做互相关（两路都按 QPC 时间戳对齐），断言 < 50ms |

参考实测值（Windows 10 19044 + VAC）：t1/t4/t5 的抑制量为 54–100dB，t6 的延迟约 31ms。

环境变量：

| 变量 | 作用 |
|---|---|
| `AUDIO_EXCLUDE_CABLE` | 虚拟声卡名，默认 `Line 1 (Virtual Audio Cable)` |
| `AUDIO_EXCLUDE_TONE_DEVICE` | 测试音使用的播放设备，默认是系统默认设备。设为另一条空闲的虚拟线后，测试就完全静音 |
| `AUDIO_EXCLUDE_MAX_LATENCY_MS` | 延迟阈值，默认 50 |
| `AUDIO_EXCLUDE_DUMP=1` | t3 打印 10ms 粒度的包络 |

## 为什么输出必须用独占模式

Process Loopback 的 EXCLUDE 模式会采集**所有端点**上的共享模式流，其中也包括 capture 自己写进虚拟声卡的那一路。
如果用共享模式输出，声音会无限回环：t3 中 200ms 的脉冲会循环数秒，电平越叠越高。
独占模式流绕过 Windows 音频引擎的混音器，Process Loopback 看不到它，回环就不会出现。

## 已知限制

- capture 运行期间**独占**虚拟声卡的播放端，其他程序不能同时往这条线播放。
- 只能排除**一棵**进程树（API 限制）。同名程序有多个独立的根进程时，只排除最早启动的那个。
- 采到的是端点音效（声卡的“增强”功能等）处理之后的声音。测试音使用 `AUDCLNT_STREAMOPTIONS_RAW` 播放，尽量绕开这些音效。
- 测试期间，系统里其他正在播放的声音会混进录音，可能影响 t1–t5 的结果，跑测试时请暂停其他音频。
- 输出队列超过 30ms 时会丢弃最旧的数据来封顶延迟（应对两个设备时钟的漂移），偶尔会有极短的毛刺。
- 进程名每 250ms 轮询一次，目标程序重启后最初约 0.25 秒的声音可能漏进虚拟声卡。
- 输出设备被拔掉或禁用时 capture 会报错退出，需要由外部重新启动它（例如计划任务）。

## License

MIT，见 [LICENSE](LICENSE)。
