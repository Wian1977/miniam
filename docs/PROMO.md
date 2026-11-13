# miniam 推广文案（复制即用）

## 一句话

在终端里听 Apple Music：真彩封面、歌名、频谱都在同一个窗口，空格暂停，n 切歌。

## 中文文案（V2EX「分享创造」/ 少数派 / 知乎 / Linux Mint 中文论坛 / 开源中国 / 掘金）

写了个终端里的 Apple Music 遥控器 **miniam**，分享给大家：

- 敲一行 `miniam` 就播，不用另开浏览器窗口，手不用离开键盘；
- 封面用 24-bit 真彩还原，歌名、进度、频谱都在同一个窗口里；
- 空格暂停 / 继续，`n` 下一首，`b` 上一首，`c` 换频谱样式，`d` 看页面诊断；
- 声音还是从你原来的音箱或耳机出来，系统默认输出不变，别的程序出声不受影响；
- 频谱只读自己这一路音频，旁边放视频不会跟着乱跳；
- 后台浏览器压住了网页里的氛围视频，机器不会因为听歌就热起来；
- Linux 二进制解压就能跑，不用装 Rust。

GitHub（含下载）：https://github.com/Wian1977/miniam/releases

## English（Reddit r/linux, r/rust, r/terminal_porn, Hacker News Show HN）

**miniam** — play Apple Music in your terminal, with true-color album art and a live spectrum.

- `miniam` starts playback; no browser window to open
- 24-bit true-color cover art, track title, progress and spectrum in one terminal
- Space to pause/resume, `n` next, `b` previous, `c` cycle spectrum style
- Audio still goes to your usual output; the system default sink stays untouched
- The spectrum reads only its own stream, so other apps' audio won't move it
- Prebuilt Linux x86_64 tarball, no Rust toolchain needed

Downloads: https://github.com/Wian1977/miniam/releases

### Show HN version

Show HN: miniam – Apple Music in a terminal with true-color cover art and spectrum.
It drives a hidden Firefox/Chrome through MusicKit (Widevine), renders the cover in
24-bit true color with Unicode quadrant blocks, and reads only its own audio stream
for the spectrum. Space pauses, n skips. Prebuilt Linux binary, source on GitHub.
https://github.com/Wian1977/miniam

## 标签 / Hashtags

#linux #rust #tui #ratatui #applemusic #terminal #cli #opensource

## 演示素材

仓库里已有 `assets/demo.gif`（内嵌在 README 顶部）和 `assets/demo.mp4`（37 秒窗口录屏），发社区时直接附上即可。

## 国内渠道

GitHub 在国内访问慢且不稳，建议同步做：

- **Gitee 镜像仓库**：导入 `https://github.com/Wian1977/miniam`，到「发行版」上传二进制包，解决国内下载慢。
- **社区**：V2EX「分享创造」、开源中国、掘金、Linux Mint 中文论坛、少数派，直接复制上方中文文案。
- **B 站**：发一段终端里切歌、封面和频谱变化的录屏，简介放仓库链接。
- **知乎**：包装成「用 Rust 给 Apple Music 写个终端遥控器」的经验文。

## GitHub 站内 / 生态曝光

- **Topics**（已设）：rust, tui, ratatui, terminal, cli, linux, apple-music, music-player, audio-visualizer, spectrum
- **About**：已填简介与 Website（Release 页）
- **投递 awesome 列表**（提 PR 把 miniam 加进清单）：awesome-rust、awesome-tuis、terminals-are-sexy、awesome-cli-apps，以及各类 Linux 桌面工具清单。

### awesome 列表 PR 文案

**标题**：`Add miniam — Apple Music in the terminal (Rust + ratatui)`

```
miniam plays Apple Music in a terminal window with true-color album art and a live spectrum.

- Starts playback with one command; controls: Space / n / b / c / d
- Cover rendered in 24-bit true color with Unicode quadrant blocks
- Spectrum reads only the controlled audio stream
- Hidden Firefox/Chrome + Widevine, system default output untouched
- Prebuilt Linux x86_64 binary

https://github.com/Wian1977/miniam
```

## 渠道现状

| 渠道 | 状态 |
|---|---|
| GitHub Releases（tar.gz / deb / 演示录屏） | 已发 v0.1.0 |
| GitHub Topics / About | 已设 |
| CI：打 tag 自动构建并上传产物 | `.github/workflows/release.yml` |
| Homebrew tap | `Wian1977/homebrew-miniam` |
| Nix (NUR) | `Wian1977/nur-packages` 的 `pkgs/miniam` |
| Gitee 镜像 / 社区发帖 / B 站 / 知乎 / awesome PR | 需网页或账号，文案已备，本人发 |
| AUR / crates.io / Flathub / PPA / Snap / Pling / KDE Store | 需网页登录或已终止，不做 |
