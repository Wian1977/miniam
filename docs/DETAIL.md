# miniam — 用终端遥控 Apple Music

一个 Rust 写的全屏 TUI 遥控器：隐藏浏览器在 `music.apple.com` 上调用 `window.MusicKit`，
终端里显示频谱、封面与走马灯标题。声音由浏览器解码后送进系统混音器。

```
miniam (Rust + ratatui，全屏 TUI，15MB 单文件)
   └─ WebDriver BiDi（Firefox）或 CDP（Chrome）──> 隐藏浏览器（music.apple.com，已登录）
                           └─ Widevine 解密 → AAC → PipeWire/PulseAudio → 系统输出
```

## 它是什么 / 不是什么

**是**：一个 TUI。频谱、封面、标题、按键控制（播放/暂停、切歌、频谱样式、页面诊断）。

**不是**播放器、不是 DRM 绕过、不是轻量版 Apple Music。音频仍由隐藏浏览器用 Widevine
解密后送进系统混音器，**那 300–400MB 级的浏览器进程树一分没少**（见「硬下限」）。

## 快速开始

```bash
cargo build --release

# 首次：会弹出可见浏览器窗口让你登录 Apple Music，登完按回车继续
miniam

# 之后：直接进界面（登录态存在 ~/.config/miniam/<browser>/ 里）
miniam
```

登录一次即可长期使用；Apple Music 的登录态由浏览器 profile 持久化。

**进界面后会自动起播**（不用再按任何键）：页面就绪（MusicKit 出队列）后自动回主页起播个人电台；
若 45 秒内页面仍未就绪（多半是没登录），也会尝试一次并给出提示。

## 按键

| 键 | 作用 |
|---|---|
| `空格` | 播放 · 暂停（`p` 仍可用作别名） |
| `n` | 下一首（点播放条上的「下一首 / 跳过」按钮） |
| `b` | 上一首（个人电台是无限流，没有上一首可退，会提示；有限专辑/歌单里有效） |
| `c` | 切换频谱样式（3 种） |
| `d` | 页面诊断（打印当前页面/MusicKit 状态到终端） |
| `q` / `Esc` | 退出（顺带关掉它拉起的浏览器） |

## 命令行

```
miniam [ui|up|reset|cover]
```

- 无参数 / `ui` —— 一键：自动拉起浏览器、进入界面（未登录时引导登录）。
- `up` —— 只把浏览器拉起来常驻（之后另开终端跑 `miniam ui` 接管）。
- `reset` —— 清理残留的 miniam / 浏览器进程与虚拟音频设备。

## 环境变量

| 变量 | 说明 |
|---|---|
| `MINIAM_BROWSER` | `firefox` 或 `chrome`，强制后端（默认自动探测） |
| `MINIAM_FIREFOX` / `MINIAM_CHROME` | 指定浏览器可执行文件路径 |
| `MINIAM_HEADLESS` | 存在则 Chrome 用 `--headless=new`（Firefox 默认就无头） |
| `MINIAM_AUDIO_SOURCE` | 强制频谱源（默认 `miniam_capture.monitor`，一般不用动） |
| `MINIAM_AMBIENT_VIDEO` | 设为 `0` 关闭「氛围视频」抑制（默认开启，见下） |
| `MINIAM_FISSION` | 设为 `1` 恢复 Firefox 站点隔离（默认关闭，省内存） |
| `MINIAM_HEVC` | 设为 `1` 恢复 Firefox 的 HEVC 解码器（默认关闭，省内存） |
| `MINIAM_CELL_ASPECT` | 终端单元格宽高比（默认 2.20，用于封面比例） |
| `MINIAM_HALFBLOCK` | 设为 `1` 启用 ▀/▌ 半块字符渲染封面（更清晰，见下） |
| `MINIAM_FONT_HEX` | 换点阵字库（默认 `assets/font_iosevka.hex`；旧 16×16 粗体在 `assets/unifont_bold.hex`） |

## 音频链路

频谱要的是**被控的那一路**音频，而不是系统里所有声音。所以：

```
浏览器播放流 --move-sink-input(按 pid 精确匹配)--> miniam_capture   ← 频谱源
                                                                    │
                                       parec 抓 monitor → FFT      │ module-loopback
                                       （只含被控那一路）           │  → 真实输出 sink
```

两条设计要点：

1. **绝不改系统默认 sink。** 默认 sink 上挂着系统里其它声音（视频、系统音），
   一旦把默认 sink 改成频谱用的虚拟 sink，那些声音就会涌进频谱源 —— 表现为「放视频时频谱跟着跳」。
2. **按 pid 识别自己的浏览器**，不按二进制名 —— 你自己的 firefox 也叫 firefox。

因此：声音设置里会多一个 `miniam-频谱采集-勿选为输出` 的虚拟输出设备，**它是内部用的，
别选它**（null sink 不出声）。退出 miniam 时它会被自动卸载。

## 氛围视频抑制（省 CPU）

Apple Music 网页版会在背景里循环播放**氛围视频**（ambient video，14 个左右，单个最大
2048×2732，源在 `mvod.itunes.apple.com`）。这些视频与音乐无关（muted、`aria-hidden`），
但 Linux 上 Firefox **只能软解 HEVC**，实测会把整个浏览器抬到约 **170% 单核** —— 在 4 核机器上
就是整机一半的 CPU，而且解码供不上会让音频断流、曲目提前结束。

所以 miniam 会在页面里装一个抑制器（定时器 + MutationObserver，持续压住新出现的氛围视频），
靶子是页面自带的 `data-testid="ambient-video-player"` 官方标记，不会碰到承载音乐的媒体元素。
实测效果（4 核机器）：

| | 抑制前 | 抑制后 |
|---|---|---|
| HEVC 解码线程 | 4~12 | **0** |
| Firefox 整棵树 | 217% 单核（整机 54%） | **8% 单核（整机 2%）** |
| 整机 CPU busy | 99% | **14%** |

关掉它：`MINIAM_AMBIENT_VIDEO=0`。

## 封面渲染（象限块真彩）

封面用「象限块」渲染：每个字符格拆成 2×2 四个子块，整格只用两种 24-bit 真彩色
（前景 fg / 背景 bg），用 Unicode 象限字符表示哪几个子块取前景色。

**采样（物理正确）**：先把图缩放到 (列数·2, 行数·4)，**用面积平均**（BOX 滤波）；
再把每格切 2×2 子块，子块内取所有像素的均值 → 每格 4 个采样点。
不能用 LANCZOS/双线性把整图降采样后直接取点 —— 插值滤波会在色块边界产生假的中间色，
这就是边缘锯齿/发虚的来源。（`image` crate 只有插值滤波，BOX 面积平均是自己用积分图实现的。）

**两色指派（精确最优）**：穷举 14 种非平凡 fg/bg 指派（掩码 1..14，排除全 0 / 全 15），
fg = 被覆盖子块均值、bg = 其余均值，SSE = 各采样点到所属色的平方误差和，取 SSE 最小者。
不用 k-means / 亮度排序 / 硬阈值二值化 / 抖动。

实测（真实专辑封面，与「理想物理图」的偏差，越小越还原）：

| | 偏差 |
|---|---|
| 旧实现（插值降采样 + 逐点取样） | 280.1 |
| 现在（面积平均 + 子块平均） | **221.5（-21%）** |

字符表按 Unicode 真实语义索引（bit0 左上 / bit1 右上 / bit2 左下 / bit3 右下）：
`[空格,▘,▝,▀,▖,▌,▞,▛,▗,▚,▐,▜,▄,▙,▟,█]`。

离线比对用 `miniam cover`（输出纯 ANSI 文本，可直接在终端 cat）：

```bash
miniam cover ~/封面.jpg out.ans 90 36
```

## 浏览器省内存（站点隔离 / HEVC 解码器）

miniam 只用 music.apple.com 一个站，Firefox 的**站点隔离**（fission）没有安全收益，
却要多开 2~3 个内容进程。所以 miniam 拉起浏览器时会写两条 pref：

- 关掉站点隔离 + 内容进程数=1 → 内容进程从 3~4 个降到 1 个
- 关掉 HEVC 解码器 → 氛围视频被抑制器压住后，解码器留着只是占着内存
  （音乐本身是 AAC/ALAC，不受影响）

实测（4 核 / 8G）：

| | 全默认 | 加上这两条 |
|---|---|---|
| Firefox 进程数 | 12 | **9** |
| Firefox RSS | ~2890M | **1800M** |
| 其中 RDD 媒体进程 | ~600M | **219M** |

恢复默认行为：`MINIAM_FISSION=1`（站点隔离）、`MINIAM_HEVC=1`（HEVC 解码器）。

## 依赖

| 类别 | 需要什么 |
|---|---|
| 音频 | `pactl`、`parec`（**PulseAudio 兼容**工具；PipeWire 由 `pipewire-pulse` 提供） |
| 浏览器 | Firefox 或 Chrome，**且带 Widevine CDM**（Apple Music 网页版 DRM 的硬要求） |
| 构建 | Rust 工具链（`cargo build --release`）；运行时只需要上面两项 |

点阵字体：`assets/font_iosevka.hex`（ASCII 用 Iosevka 8×16，汉字用文泉驿）编译期内置。

## 常见故障速查

| 现象 | 多半是 |
|---|---|
| 频谱没反应 | 浏览器没在播放；或浏览器流没被接进频谱源。查 `pactl list sink-inputs` 里有没有我们那个浏览器的流、`/tmp/miniam_audio.log` 是否有「未能识别浏览器进程」 |
| 放视频时频谱跟着动 | 有别的流（你自己的 firefox / 播放器）被留在频谱源里了。查 `pactl list sink-inputs` 对比 `application.process.id` 与 `/proc/<pid>/cmdline`（我们的浏览器带 `--profile ~/.config/miniam/`） |
| 完全没声音 | ① 默认 sink 被残留污染（跑 `miniam reset`）；② 无声卡服务器需修 `/dev/snd` 的 ACL 或加入 `audio` 组；③ 真实输出 sink 不存在 |
| 登录窗口不弹 | 浏览器没真起来（profile 被上次残留占用）；或会话没有图形显示（需要 DISPLAY/WAYLAND_DISPLAY，或 `ssh -X`） |
| 按 q/Esc 桌面被注销 | 已在 2026-11-12 修复（旧版用 `kill -{pid}` 组杀，会误伤图形会话组长） |
| 起播是旧歌单、跳完就没歌了 | 已在 2026-11-11 修复（起播先回主页播个人电台 = 无限流） |

## 硬下限（再强调一次）

Apple Music 网页版的 DRM（Widevine）决定了：**必须有真浏览器、且必须能解密**，
300–400MB 的浏览器进程树省不掉。能省的只是控制面那十几 MB，和浏览器本身的启动参数与
内存占用（Firefox 侧靠 `dom.ipc.processCount=1`、关磁盘缓存等 user.js 项压）。

换轻浏览器（WebKitGTK / Servo）或自托管一个简化页面都会破坏 Widevine 与 origin 绑定，
这条路上已经验证过走不通。
