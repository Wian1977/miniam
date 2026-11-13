# miniam

One command to play Apple Music in your terminal. Keep it open while coding, reading logs or waiting for a build; no browser window to open, no webpage to click for the next track.

一行命令，在终端里听 Apple Music。适合写代码、看日志、跑构建的时候开着。不用另开浏览器窗口，也不用为切歌去点网页。

![miniam running](https://github.com/Wian1977/miniam/releases/download/v0.1.0/demo.gif)

## Features
### 功能特点

- True-color album art — 24-bit color through Unicode quadrant blocks (`▘ ▝ ▀ ▖ ▌ ▞ ▛ …`)
- Live spectrum — several styles that follow the music
- Full playback controls — play/pause, next, previous
- A hidden Firefox/Chrome window plays Apple Music in the background
- The spectrum reads only its own audio stream, so other apps' sound won't move it

- **真彩专辑封面** — 使用 Unicode 象限字符 (`▘ ▝ ▀ ▖ ▌ ▞ ▛ …`) 渲染 24-bit 真彩色封面
- **动态频谱可视化** — 多种频谱样式，跟随音乐起伏
- **完整播放控制** — 播放/暂停、上一首、下一首
- **隐藏浏览器** — 后台用隐藏的 Firefox/Chrome 窗口播放 Apple Music
- **系统音频捕获** — 通过 PulseAudio/PipeWire 只捕获自己这一路音频做频谱分析

## Quick start
### 快速开始

```bash
cargo build --release
./target/release/miniam
```

A browser window opens on first launch to sign in to Apple Music; sign in once and it stays valid.

首次启动会弹出浏览器窗口让你登录 Apple Music，登录完成后按回车继续。之后可直接进入界面。

## Keyboard
### 键盘控制

| Key | Action / 作用 |
|---|---|
| `Space` | Play / Pause · 播放 / 暂停 |
| `n` | Next · 下一首 |
| `b` | Previous · 上一首 |
| `c` | Cycle spectrum style · 切换频谱样式 |
| `d` | Page diagnostics · 页面诊断 |

## Configuration
### 环境配置

### System requirements
### 系统要求

- Linux + terminal (PipeWire or PulseAudio)
- Firefox or Chrome (official build)
- Rust toolchain (build only)

- Linux + 终端 (PipeWire 或 PulseAudio)
- Firefox 或 Chrome（官方构建）
- Rust 工具链 (仅构建需要)

### Environment variables
### 环境变量

| Variable | 说明 / Description | Default / 默认值 |
|---|---|---|
| `MINIAM_BROWSER` | 强制后端 firefox / chrome · Force backend | auto · 自动探测 |
| `MINIAM_FIREFOX` / `MINIAM_CHROME` | 浏览器可执行文件路径 · Browser binary | auto · 自动 |
| `MINIAM_HEADLESS` | Chrome 无头模式 · Chrome headless | off · 关 |
| `MINIAM_AUDIO_SOURCE` | 频谱源 · Spectrum source | `miniam_capture.monitor` |
| `MINIAM_AMBIENT_VIDEO` | `0` 关闭氛围视频抑制 · `0` disables ambient-video guard | on · 开 |
| `MINIAM_FISSION` | `1` 恢复站点隔离 · `1` restores site isolation | off · 关 |
| `MINIAM_HEVC` | `1` 恢复 HEVC 解码 · `1` restores HEVC decoder | off · 关 |
| `MINIAM_CELL_ASPECT` | 单元格宽高比 · Cell aspect ratio | 2.20 |
| `MINIAM_HALFBLOCK` | `1` 启用半块字符封面 · `1` uses half-block cover | off · 关 |
| `MINIAM_FONT_HEX` | 换点阵字库 · Alternate bitmap font | `font_iosevka.hex` |

### Audio routing
### 音频链路

The browser stream is moved into a virtual sink (`miniam_capture`) and looped back to the real output device. The system default sink stays untouched.

程序创建虚拟音频接收器，用 PulseAudio/PipeWire 把浏览器音频搬进去，再回环到真实输出设备；系统默认输出保持原样。

## Spectrum styles
### 频谱样式

- Bar — classic bars with particles
- Radial — radial rays with a glow core

Press `c` to cycle.

- **Bar 频谱** — 传统柱状图样式，带粒子效果
- **Radial 频谱** — 径向放射样式，带光晕效果

按 `c` 键可在不同样式间切换。

## FAQ
### 常见问题

**Q: How do I continue after the first login? / 首次登录后如何继续？**
A: Press Enter in the terminal after signing in. / 登录完成后在终端按回车键继续。

**Q: Previous track does nothing on personal radio? / 播放个人电台时按上一首无反应？**
A: Personal radio is an endless stream with no previous track; pressing `b` will tell you so. / 个人电台是无限流，没有上一首，此时按 `b` 会提示。

**Q: The terminal looks wrong? / 终端显示异常怎么办？**
A: Use a terminal with 24-bit color. Ghostty / Kitty / WezTerm / Alacritty work well. Some CJK locales render Unicode quadrant characters as double-width. / 确保终端支持真彩色 (24-bit)。Ghostty / Kitty / WezTerm / Alacritty 等终端表现良好。部分 CJK 语言环境下 Unicode 象限字符可能被当作双宽字符渲染。

**Q: The spectrum does not move? / 音频可视化不工作？**
A: Check that PipeWire/PulseAudio is running and audio is playing. / 检查 PulseAudio/PipeWire 服务是否正常运行，确保有音频在播放。

## Project layout
### 项目结构

```
miniam/
├── src/
│   ├── main.rs           # 主程序逻辑、TUI、音频处理
│   ├── bitmap_font.rs    # 点阵字体渲染
│   └── braille_text.rs   # 盲文点阵文字渲染
├── assets/
│   ├── font_iosevka.hex  # 默认点阵字库（Iosevka + 文泉驿）
│   ├── font12.hex        # 早期默认字库
│   ├── unifont.hex       # 备选字库
│   └── unifont_bold.hex  # 16×16 粗体备选
├── docs/
│   └── DETAIL.md         # 详细文档
└── Cargo.toml            # Rust 项目配置
```

## Implementation
### 技术实现

- TUI: ratatui
- Image processing: image crate
- Browser control: WebDriver BiDi / CDP
- Audio: PulseAudio / PipeWire capture

- **TUI 框架**: ratatui
- **图像处理**: image crate
- **浏览器控制**: WebDriver / BiDi 协议
- **音频处理**: 通过 PulseAudio API 捕获实时音频

## Disclaimer
### 免责声明

miniam is an independent, unofficial project and is not affiliated with Apple Inc. Apple Music is a trademark of Apple Inc. It plays through the official web player in a standard browser using your own subscription.

miniam 是独立的非官方项目，与 Apple Inc. 无关。Apple Music 为 Apple Inc. 的商标。它通过标准浏览器中的官方网页播放器使用你自己的订阅账号。

## License
### 许可证

MIT. Bundled bitmap fonts keep their own licenses; see `assets/README-unifont.txt`.

代码采用 MIT 许可。内置点阵字库各有独立许可，见 `assets/README-unifont.txt`。
