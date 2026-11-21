# audio-exclude

Windows 工具：用 WASAPI process loopback 采集“除某个程序外”的全部系统声音，写进虚拟声卡（默认 `Line 1 (Virtual Audio Cable)`），被控端的 RustDesk 把 Audio input 设成这条线的录音端。用法、参数、每个测试用例的断言、测试用的环境变量都在 `README.md`，这里只记录看代码看不出来的东西。

## 协作约定

- 用户用中文交流。这是长期项目，用户通常每个功能开一个新会话。
- 仓库是私有的 `xuxian-pw/Ausk`（MIT）。每个里程碑提交后可以直接 `git push`；改成公开或改可见性要先问用户。
- bash 的 PATH 里没有 `gh`，要用 `C:\Program Files\GitHub CLI\gh.exe`。
- 最终验收是用户通过 RustDesk 实际听：同一个场景，把 Audio input 分别设成 System sound 和 Line 1，对比两边。测试通过只说明可以交给用户去听了。

## 跑测试

- 测试使用真实设备，会从默认输出播放测试音，并独占 Line 1。用户的 `tray.exe` 在运行时，测试打不开 Line 1，`cargo build --release` 也写不进 exe。遇到这种情况，请用户从托盘菜单点“退出”，由用户自己关，不要结束这个进程。
- 测试强度适中：针对性的诊断跑一两次，完整测试跑一两次确认就够，不要连续跑十几轮做统计。
- 用户装了 ESET。新进程打开录音设备时，ESET 可能播放约 2.5 秒的提示音，这段声音会被采进 Line 1。测试在开始测量前会先等系统安静下来；分析没解释清楚的杂音时，先排除这个来源。
- 名字是 `diag_*` 的测试默认被忽略，是一次性的排查工具，需要时用 `--ignored` 单独运行。

## 音频管线

修改采集、输出、缓冲、时间线或漂移控制的代码（`wasapi.rs`、`engine.rs`、`output.rs`、`timeline.rs`），或者排查杂音、断点、延迟之前，先读 `docs/audio-notes.md`。里面记录的是测量和试听确认过的结论，包括输出为什么必须独占、断点为什么只能补静音。
