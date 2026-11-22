# Ausieve

Windows 工具：用 WASAPI process loopback 采集“除某个程序外”的全部系统声音，写进虚拟声卡（默认 `Line 1 (Virtual Audio Cable)`），被控端的 RustDesk 把 Audio input 设成这条线的录音端。用法和参数在 `README.md`，测试用例的断言和测试用的环境变量在 `docs/testing.md`，延迟构成和各次杂音修复的实测在 `docs/design.md`，这里只记录看代码看不出来的东西。

## 约定

- 文档、注释以外的说明和提交讨论用中文。
- 最终验收是通过 RustDesk 实际听：同一个场景，把 Audio input 分别设成 System sound 和 Line 1，对比两边。测试通过只说明可以拿去试听了。

## 排障

使用者报告远程没有声音或声音不对时，先读 `docs/troubleshooting.md`：里面是目标、已知事实和陷阱，怎么查自己判断。测量可以用 `scripts/diagnose.ps1`。`scripts/` 是开发工具，不放进安装包。

## 跑测试

- 测试使用真实设备，会从默认输出播放测试音，并独占 Line 1。`Ausieve.exe` 在运行时，测试打不开 Line 1，`cargo build --release` 也写不进 exe。遇到这种情况，请使用者从托盘菜单点“退出”，不要直接结束这个进程。
- 测试强度适中：针对性的诊断跑一两次，完整测试跑一两次确认就够，不要连续跑十几轮做统计。
- 有的安全软件（例如 ESET）在新进程打开录音设备时会播放约 2.5 秒的提示音，这段声音会被采进 Line 1。测试在开始测量前会先等系统安静下来；分析没解释清楚的杂音时，先排除这个来源。
- 名字是 `diag_*` 的测试默认被忽略，是一次性的排查工具，需要时用 `--ignored` 单独运行。

## 发布

- 发布用的安装包一律用 `installer/build.ps1` 生成。它通过 `RUSTFLAGS` 的 `--remap-path-prefix` 把 cargo 目录和项目目录换成中性路径，否则 exe 里会带上构建者的用户名（依赖库的 panic 信息里有源文件的绝对路径）。不要把这些路径写进仓库里的配置文件。

## 音频管线

修改采集、输出、缓冲、时间线或漂移控制的代码（`wasapi.rs`、`engine.rs`、`output.rs`、`timeline.rs`），或者排查杂音、断点、延迟之前，先读 `docs/audio-notes.md`。里面记录的是测量和试听确认过的结论，包括输出为什么必须独占、断点为什么只能补静音。
