# miniam

One command to play Apple Music in your terminal.

一行命令，在终端里听 Apple Music。

True-color album art, track title and spectrum, all without leaving the keyboard.

真彩封面、歌名、频谱一个不少，手不用离开键盘。

![miniam running / miniam 运行时的窗口](https://github.com/Wian1977/miniam/releases/download/v0.1.0/demo.gif)

Keep it open while coding, reading logs or waiting for a build; no browser window to open, no webpage to click for the next track.

适合写代码、看日志、跑构建的时候开着。不用另开浏览器窗口，也不用为切歌去点网页。

Playback starts on its own. Space pauses; Space resumes.

启动后自动接着播放，空格暂停，再按继续。

## What you see
### 打开能看到什么

From top to bottom: album art, the current track and artist, progress, a spectrum that follows the music, and queue status at the bottom. Cover colors come from the original image in 24-bit true color.

从上到下是专辑封面、正在播的歌名和艺人、进度、随音乐起伏的频谱，最下面一行是队列状态。封面按终端字符格还原成 24-bit 真彩色，颜色取自原图；

## Sound and machine
### 声音和机器

Sound still comes out of your usual speakers or headphones; the system default output stays as it was, and other apps keep their audio.

歌从你平时用的音箱或耳机出来，系统默认输出保持原样，别的程序该出声还出声。

It uses your own Apple Music account, so playlists and audio quality match the web player. Log in once and it keeps working.

用的还是你自己的 Apple Music 账号，歌单和音质跟网页里一样，登录一次就长期有效。

The background browser is tuned to use very little of the machine.

后台的浏览器充分优化过，极少占用系统资源；

## Usage
### 用法

```bash
miniam
```

| Key | Action<br>作用 |
|---|---|
| `Space` | Play / Pause<br>播放 / 暂停 |
| `n` | Next<br>下一首 |
| `b` | Previous<br>上一首 |
| `c` | Cycle spectrum style<br>换频谱样式 |
| `d` | Page diagnostics<br>页面诊断 |

Personal radio is an endless stream with no previous track; pressing `b` will tell you so.

个人电台是无限流，没有上一首可退，这时按 `b` 会提示。

The first launch opens a browser window; sign in with your Apple account as usual.

首次启动会弹出浏览器页面，正常使用 apple 帐号登录即可。

## Requirements
### 需要什么

- Linux + terminal (PipeWire or PulseAudio)
- Firefox or Chrome with Widevine CDM
- Rust toolchain (build only)

- Linux + 终端（PipeWire / PulseAudio 都行）
- Firefox 或 Chrome，带 Widevine CDM
- Rust 工具链（构建用）

```bash
cargo build --release
```

Environment variables, audio routing, cover rendering and troubleshooting live in [docs/DETAIL.md](docs/DETAIL.md).

环境变量、音频链路、封面渲染和排障写在 [docs/DETAIL.md](docs/DETAIL.md)。

## Notes
### 其他

Apple Music handles decoding, DRM and subscriptions; miniam only pulls the stream and draws it. Sound quality depends on your account.

解码、加密、订阅都由 Apple Music 处理，miniam 只做取流和显示。声音质量取决于你的账号。

Cover rendering uses a few Unicode quadrant characters (`▘ ▝ ▀ ▖ ▌ ▞ ▛ …`). Most terminals draw them single-width; some CJK locales treat them as double-width. Ghostty / Kitty / WezTerm / Alacritty work fine.

封面渲染用到少量 Unicode 象限字符（`▘ ▝ ▀ ▖ ▌ ▞ ▛ …`）。多数终端里它们是等宽的，部分 CJK 语言环境下可能被当作双宽字符渲染。Ghostty / Kitty / WezTerm / Alacritty 下正常。

## License
### 许可

Use the code freely. Licenses for the bundled bitmap fonts are in `assets/README-unifont.txt`.

代码可自由取用。内置点阵字库的许可见 `assets/README-unifont.txt`。
