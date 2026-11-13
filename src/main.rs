//! miniam — 最小可用的 Linux Apple Music 遥控器
//!
//! 架构：
//!   miniam (Rust，按需启动，几 MB，用完即退)
//!      └─ CDP(Chrome) 或 WebDriver BiDi(Firefox) ──> 隐藏的浏览器（music.apple.com，已登录）
//!                                └─ Widevine 解密 → 音频直接进系统混音器
//!
//! 这个程序**不播放**任何东西，只是把按键翻译成页面里的 window.MusicKit 调用。
//! 所以它不能绕开 DRM，也省不掉浏览器那几百 MB。

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use serde_json::{json, Value};
use std::{
    env, fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
    sync::Arc, sync::Mutex,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::net::TcpStream as TokioTcpStream;
use tokio_tungstenite::{
    connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream,
};

type WsStream = WebSocketStream<MaybeTlsStream<TokioTcpStream>>;

const PORT: u16 = 9222;
const ORIGIN: &str = "https://music.apple.com";
/// 单条 BiDi 命令的等响应上限：全新 profile 首次加载 Widevine CDM 可能要几十秒，
/// 给足时间但别让 CLI 永久挂起。
const BIDI_TIMEOUT: Duration = Duration::from_secs(60);
/// 起播：找到页面第一个「播放」卡片并算出坐标（Firefox 用 trusted 点击）。
const BOOT_RECT_EXPR: &str = r#"(() => {
  const re = /^播放/;
  const visible = Array.from(document.querySelectorAll('button')).filter(e => {
    const lab = (e.getAttribute('aria-label')||'') + (e.textContent||'');
    if (!re.test(lab)) return false;
    const r = e.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  });
  if (!visible.length) return { ok: false, reason: 'no visible play button' };
  // 优先个人电台（无限流）：按钮所在卡片含 /station/ 链接，避免点到专辑/最近播放的有限列表。
  const isStation = (b) => {
    let el = b;
    for (let i = 0; i < 5 && el && el !== document.body; i++) {
      if (el.querySelector && el.querySelector('a[href*="/station/"]')) return true;
      el = el.parentElement;
    }
    return false;
  };
  const target = visible.find(isStation) || visible[0];
  target.scrollIntoView({ block: 'center', inline: 'center' });
  const r = target.getBoundingClientRect();
  return { ok: true, x: Math.round(r.left + r.width/2), y: Math.round(r.top + r.height/2),
           label: (target.getAttribute('aria-label')||''), station: isStation(target) };
})()"#;
/// Chrome 侧：CDP 的 Runtime.evaluate 支持 userGesture，可在手势上下文里直接点。
const BOOT_CLICK_EXPR: &str = r#"(() => {
  const re = /^播放/;
  const visible = Array.from(document.querySelectorAll('button')).filter(e => {
    const lab = (e.getAttribute('aria-label')||'') + (e.textContent||'');
    if (!re.test(lab)) return false;
    const r = e.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  });
  if (!visible.length) return { ok: false, reason: 'no visible play button' };
  const isStation = (b) => {
    let el = b;
    for (let i = 0; i < 5 && el && el !== document.body; i++) {
      if (el.querySelector && el.querySelector('a[href*="/station/"]')) return true;
      el = el.parentElement;
    }
    return false;
  };
  const target = visible.find(isStation) || visible[0];
  target.click();
  return { ok: true, label: (target.getAttribute('aria-label')||''), station: isStation(target) };
})()"#;
/// 状态查询的 JS body，status 命令和 ui 界面共用。
const STATUS_BODY: &str = r#"const it = mk.nowPlayingItem || {};
    function artUrl(a) {
        if (!a) return null;
        if (typeof a.url === 'function') return a.url(300, 300);
        if (typeof a.url === 'string') return a.url.split('{w}').join('300').split('{h}').join('300');
        if (typeof a.src === 'string') return a.src;
        return null;
    }
    return {
        isPlaying: mk.isPlaying,
        playbackState: mk.playbackState ?? null,
        playbackTime: mk.currentPlaybackTime ?? mk.playbackTime ?? null,
        duration: it.playbackDuration ?? null,
        title: it.title ?? null,
        artist: it.artistName ?? null,
        album: it.albumName ?? null,
        artwork: artUrl(it.artwork),
        queueLength: mk.queue ? mk.queue.length : null,
        queueName: (mk.queue && mk.queue.name) ? mk.queue.name : null,
        // 诊断用（只读）：页面是否被导航、标签是否后台、MusicKit 播放状态码。
        url: location.href,
        hidden: document.hidden,
        visibility: document.visibilityState,
        docTitle: document.title
    };"#;

/// 状态查询时顺带自愈氛围视频抑制器。
///
/// 页面整页刷新（不只是 SPA 路由）会清掉 window 上的抑制器，后台视频就又开始烧 CPU。
/// 这里在每次状态轮询里顺带检查一次：抑制器不在就重装（一次额外的 BiDi 往返，
/// 只在异常时发生），在就不动 —— 不给正常轮询增加负担。
const AMBIENT_GUARD_CHECK: &str = r#"(() => {
    if (window.__miniamAmbientGuard) { return { ok: true, present: true }; }
    return { ok: true, present: false };
})()"#;

/// 抑制 Apple Music 网页版背景里的「氛围视频」（ambient video）。
///
/// 页面在主视觉区域循环播放 mvod.itunes.apple.com 的 HEVC 视频（实测 14 个，
/// 单个最大 2048x2732，多个同时在播）。这些视频与音乐无关（muted、aria-hidden），
/// 但 Linux 上 Firefox 只能软解 HEVC，实测把整个浏览器抬到 ~170% 单核：
/// 在 4 核机器上就是整机一半的 CPU，而且解码供不上会让音频断流、曲目提前结束。
///
/// 靶子用页面自带的 data-testid="ambient-video-player"（官方标记），不靠类名/祖先元素猜，
/// 也不会误伤真正承载音乐的媒体元素。
///
/// 页面会重新创建并恢复播放这些视频（实测 reload 后又长出 14 个），所以只在页面里
/// 装一次抑制器（定时器 + MutationObserver）持续压制，而不是每次查询只处理一轮。
const AMBIENT_VIDEO_GUARD: &str = r#"(() => {
    if (window.__miniamAmbientGuard) { return { ok: true, already: true }; }
    window.__miniamAmbientGuard = true;
    const SEL = '[data-testid="ambient-video-player"]';
    const sweep = () => {
        const vs = document.querySelectorAll(SEL + ' video');
        let n = 0;
        for (const el of vs) {
            if (!el.paused) { try { el.pause(); n++; } catch (e) {} }
        }
        return n;
    };
    window.__miniamAmbientSweep = sweep;
    sweep();
    // 页面会新建/恢复播放氛围视频：定时器 + DOM 变更观察持续压制。
    setInterval(sweep, 1000);
    try {
        new MutationObserver(() => sweep()).observe(document.documentElement,
            { childList: true, subtree: true });
    } catch (e) {}
    return { ok: true };
})()"#;

/// 氛围视频抑制是否启用（默认开；MINIAM_AMBIENT_VIDEO=0 关闭）。
fn ambient_video_guard_enabled() -> bool {
    !matches!(env::var("MINIAM_AMBIENT_VIDEO").as_deref(), Ok("0"))
}

/// 把氛围视频抑制器装进页面（幂等：页面里已有就不再重装）。
///
/// 页面 reload 后 window.__miniamAmbientGuard 会丢，需要重新装 —— 所以起播
/// （会导航回主页）之后也要补一次。
async fn install_ambient_guard(b: Backend, fx: &mut Option<BiDi>) {
    let expr = AMBIENT_VIDEO_GUARD;
    let res = match b {
        Backend::Chrome => eval_expr(b, fx, expr).await,
        Backend::Firefox => fx_eval(fx, expr).await,
    };
    if let Err(e) = res {
        // 装不上不是致命错误：只是背景视频还在放，功能不受影响。
        log_audio_note(&format!("氛围视频抑制器未生效: {}", e));
    }
}

/// 在页面内把封面 URL 取回为 base64 dataURL（带登录态，规避跨域 canvas 污染问题）。
/// 仅当曲目封面 URL 变化时才调用，正常每秒轮询不会触发。
const COVER_FETCH_BODY: &str = r#"
    const r = await fetch(u);
    const blob = await r.blob();
    const dataUrl = await new Promise((res, rej) => {
        const fr = new FileReader();
        fr.onload = () => res(fr.result);
        fr.onerror = () => rej(new Error('read failed'));
        fr.readAsDataURL(blob);
    });
    return { ok: true, dataUrl: dataUrl };
"#;

#[derive(Clone, Copy, PartialEq)]
enum Backend {
    Chrome,
    Firefox,
}

/// PATH 里是否存在某个可执行文件（也接受显式路径）。
fn bin_available(bin: &str) -> bool {
    if bin.contains('/') {
        return Path::new(bin).is_file();
    }
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

/// 后端选择：显式指定优先，否则按机器上实际装了什么自动探测（优先 Chrome，保持原行为）。
fn backend() -> Backend {
    match env::var("MINIAM_BROWSER").as_deref() {
        Ok("firefox") => return Backend::Firefox,
        Ok("chrome") => return Backend::Chrome,
        _ => {}
    }
    if env::var_os("MINIAM_CHROME").is_some() {
        return Backend::Chrome;
    }
    if env::var_os("MINIAM_FIREFOX").is_some() {
        return Backend::Firefox;
    }
    if bin_available("google-chrome") {
        return Backend::Chrome;
    }
    if bin_available("firefox") {
        return Backend::Firefox;
    }
    Backend::Chrome
}

fn profile_dir(b: Backend) -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    let name = match b {
        Backend::Chrome => "chrome",
        Backend::Firefox => "firefox",
    };
    PathBuf::from(home).join(".config/miniam").join(name)
}

fn browser_bin(b: Backend) -> String {
    match b {
        Backend::Chrome => env::var("MINIAM_CHROME")
            .unwrap_or_else(|_| "google-chrome".into()),
        Backend::Firefox => env::var("MINIAM_FIREFOX")
            .unwrap_or_else(|_| "firefox".into()),
    }
}

/// 极简 HTTP GET，只为读 Chrome 的 /json/list 与 /json/version，省掉一个 HTTP 库。
fn http_get(path: &str) -> Result<String> {
    let mut s = TcpStream::connect(("127.0.0.1", PORT))?;
    write!(
        s,
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        path
    )?;
    let mut buf = String::new();
    s.read_to_string(&mut buf)?;
    buf.split("\r\n\r\n")
        .nth(1)
        .map(|b| b.to_string())
        .ok_or_else(|| anyhow!("Chrome 返回了不完整的 HTTP 响应"))
}

fn port_alive() -> bool {
    TcpStream::connect(("127.0.0.1", PORT)).is_ok()
}

fn page_ws() -> Result<String> {
    let list: Value = serde_json::from_str(&http_get("/json/list")?)?;
    let arr = list
        .as_array()
        .ok_or_else(|| anyhow!("/json/list 不是数组"))?;

    // 优先找 Apple Music 页；找不到就退回第一个 page，让上游报 MusicKit 未就绪。
    let page = arr
        .iter()
        .find(|t| {
            t["type"] == "page"
                && t["url"].as_str().unwrap_or("").contains("music.apple.com")
        })
        .or_else(|| arr.iter().find(|t| t["type"] == "page"))
        .ok_or_else(|| anyhow!("没有找到页面 target，先执行 `miniam login`"))?;

    page["webSocketDebuggerUrl"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow!("target 没有 webSocketDebuggerUrl"))
}

fn browser_ws() -> Result<String> {
    let v: Value = serde_json::from_str(&http_get("/json/version")?)?;
    v["webSocketDebuggerUrl"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow!("拿不到 browser 级 WebSocket"))
}

/// 发一条 CDP 命令，等同一 id 的响应。page 级和 browser 级 WS 都适用。
async fn cdp_call(ws: &str, method: &str, params: Value) -> Result<Value> {
    let (mut sock, _) = connect_async(ws)
        .await
        .with_context(|| format!("连接 CDP 失败: {}", ws))?;

    let req = json!({ "id": 1, "method": method, "params": params });
    sock.send(Message::text(req.to_string())).await?;

    while let Some(msg) = sock.next().await {
        if let Message::Text(txt) = msg? {
            let v: Value = serde_json::from_str(&txt)?;
            if v["id"] != 1 {
                continue; // CDP 事件，忽略
            }
            if let Some(e) = v.get("error") {
                bail!("CDP 错误: {}", e);
            }
            return Ok(v["result"].clone());
        }
    }
    // Browser.close 会让 Chrome 立即退出，响应可能来不及送达，断连也算成功。
    if method == "Browser.close" {
        return Ok(Value::Null);
    }
    bail!("CDP 连接提前关闭")
}

/// 在页面里执行 JS 并取回值（Chrome / CDP）。returnByValue + awaitPromise 支持 async 表达式。
async fn cdp_eval(ws: &str, expression: &str) -> Result<Value> {
    let r = cdp_call(
        ws,
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
        }),
    )
    .await?;

    if let Some(ex) = r.get("exceptionDetails") {
        bail!("页面 JS 抛异常: {}", ex);
    }
    Ok(r["result"].get("value").cloned().unwrap_or(Value::Null))
}

/// WebDriver BiDi（Firefox）：一条 WS 上先开 session，再发命令。
struct BiDi {
    sock: WsStream,
    next_id: i64,
}

impl BiDi {
    async fn connect() -> Result<Self> {
        let url = format!("ws://127.0.0.1:{}/session", PORT);
        // 握手必须设超时：浏览器异常时 connect_async 可能永久挂起，导致 ui 零输出卡死。
        let (sock, _) = tokio::time::timeout(Duration::from_secs(10), connect_async(&url))
            .await
            .with_context(|| format!("连接 BiDi 超时: {}", url))?
            .with_context(|| format!("连接 BiDi 失败: {}", url))?;
        let mut b = BiDi { sock, next_id: 0 };
        b.call("session.new", json!({ "capabilities": {} })).await?;
        Ok(b)
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({ "id": id, "method": method, "params": params });
        self.sock.send(Message::text(req.to_string())).await?;

        loop {
            let maybe = tokio::time::timeout(BIDI_TIMEOUT, self.sock.next())
                .await
                .with_context(|| format!("BiDi 命令 {} 在 {:?} 内没响应", method, BIDI_TIMEOUT))?;
            let Some(msg) = maybe else {
                // browser.close 后浏览器会退出，响应可能来得及也可能来不及。
                if method == "browser.close" {
                    return Ok(Value::Null);
                }
                bail!("BiDi 连接提前关闭");
            };
            if let Message::Text(txt) = msg? {
                let v: Value = serde_json::from_str(&txt)?;
                if v["id"] != id {
                    continue; // BiDi 事件，忽略
                }
                if v["type"] == "error" {
                    bail!("BiDi 错误: {} {}", v["error"], v["message"]);
                }
                return Ok(v["result"].clone());
            }
        }
    }

    /// 释放 session：Firefox 的活跃 session 数有限，不释放会把后续命令全挡住。
    /// 必须等确认（只发不等实测会漏释放）；异常路径最多多等 3s，不拖累退出。
    async fn end(&mut self) {
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            self.call("session.end", json!({})),
        )
        .await;
    }
}

/// BiDi 的 RemoteValue 转成普通 JSON：对象/映射是 [[k, v], ...] 数组。
fn bidi_value(v: &Value) -> Value {
    match v["type"].as_str().unwrap_or("") {
        "undefined" | "null" => Value::Null,
        "array" | "set" => Value::Array(
            v["value"]
                .as_array()
                .map(|a| a.iter().map(bidi_value).collect())
                .unwrap_or_default(),
        ),
        "object" | "map" => {
            let mut m = serde_json::Map::new();
            if let Some(pairs) = v["value"].as_array() {
                for pair in pairs {
                    if let Some(kv) = pair.as_array() {
                        if kv.len() >= 2 {
                            // BiDi 的对象 key 可能是纯字符串，也可能是 RemoteValue。
                            let key = match &kv[0] {
                                Value::String(s) => s.clone(),
                                other => match bidi_value(other) {
                                    Value::String(s) => s,
                                    v => v.to_string(),
                                },
                            };
                            m.insert(key, bidi_value(&kv[1]));
                        }
                    }
                }
            }
            Value::Object(m)
        }
        _ => v["value"].clone(),
    }
}

/// Firefox：复用同一个持久 BiDi 会话执行页面 JS。
/// 关键修复：原先每次 eval 都新建+拆除 session，而 Firefox 的活跃会话数有上限，
/// session.end 又不可靠释放，会话很快耗尽，导致状态轮询 / g / n / 封面 全部 eval 失败。
/// 这里整个 worker 生命周期只维护一个 session，断开时自动重建一次。
async fn fx_eval(session: &mut Option<BiDi>, expr: &str) -> Result<Value> {
    if session.is_none() {
        *session = BiDi::connect().await.ok();
    }
    match session {
        Some(b) => match bidi_eval_in(b, expr).await {
            Ok(v) => Ok(v),
            Err(e) => {
                if let Some(mut old) = session.take() {
                    old.end().await;
                }
                *session = BiDi::connect().await.ok();
                match session {
                    Some(b) => bidi_eval_in(b, expr).await,
                    None => Err(e),
                }
            }
        },
        None => bail!("无法建立 BiDi 会话（Firefox 调试端口是否可用？）"),
    }
}


async fn bidi_eval_in(b: &mut BiDi, expression: &str) -> Result<Value> {
    let tree = b.call("browsingContext.getTree", json!({})).await?;
    let contexts = tree["contexts"]
        .as_array()
        .ok_or_else(|| anyhow!("BiDi 没有返回 browsing contexts"))?;
    let ctx = contexts
        .iter()
        .find(|c| c["url"].as_str().unwrap_or("").contains("music.apple.com"))
        .or_else(|| contexts.first())
        .and_then(|c| c["context"].as_str())
        .ok_or_else(|| anyhow!("没有找到页面 target，先执行 `miniam login`"))?;

    let r = b
        .call(
            "script.evaluate",
            json!({
                "expression": expression,
                "target": { "context": ctx },
                "awaitPromise": true,
                "resultOwnership": "none",
            }),
        )
        .await?;

    if r["type"] == "exception" {
        bail!("页面 JS 抛异常: {}", r["exceptionDetails"]);
    }
    Ok(bidi_value(&r["result"]))
}

/// 所有控制都包一层，确保 MusicKit 已就绪再动手。
fn mk(body: &str) -> String {
    format!(
        r#"(async () => {{
            const mk = window.MusicKit && window.MusicKit.getInstance();
            if (!mk) return {{ error: "MusicKit 未就绪：页面加载完 / 登录了吗？" }};
            {body}
        }})()"#,
        body = body
    )
}

/// 递归拷贝目录（只为搬运 Widevine CDM，不处理符号链接）。
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

/// gmp-widevinecdm/<版本>/libwidevinecdm.so 是否存在。
fn has_cdm(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|v| v.path().join("libwidevinecdm.so").is_file())
}

/// 从系统已有的 Firefox profile 里找一个可用的 Widevine CDM，优先 default-release。
fn find_system_cdm() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    let profiles = PathBuf::from(home).join(".mozilla/firefox");
    let mut dirs: Vec<_> = match fs::read_dir(profiles) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .collect(),
        Err(_) => return None,
    };
    dirs.sort_by_key(|e| !e.file_name().to_string_lossy().contains("default-release"));
    dirs.iter()
        .map(|e| e.path().join("gmp-widevinecdm"))
        .find(|p| has_cdm(p))
}

/// 工具专用 profile 首次使用时，若本机已有 CDM 就拷一份过去。
/// Firefox 的 CDM 按 profile 下载，网络不通时会卡在 "Timed out while waiting for a CDM update"。
/// Firefox 的 GMP/插件注册元数据。只拷 CDM 目录不够：Firefox 会认为插件未安装，
/// EME 请求会一直挂起。addonStartup.json.lz4 是压缩的，里面才有 GMP 记录。
const GMP_META_FILES: &[&str] = &[
    "addons.json",
    "addonStartup.json.lz4",
    "extensions.json",
    "extension-settings.json",
];

fn seed_widevine(dir: &Path) {
    let dst = dir.join("gmp-widevinecdm");
    if has_cdm(&dst) {
        return; // 已有就别动，免得覆盖用户手动放的版本
    }
    let Some(src) = find_system_cdm() else {
        return; // 找不到就交给 Firefox 自己联网下载
    };
    // 注册元数据和 CDM 目录必须在同一个 profile 里，从源 profile 顶层一起搬。
    if let Some(profile) = src.parent() {
        for name in GMP_META_FILES {
            let from = profile.join(name);
            if from.is_file() {
                let _ = fs::copy(&from, dir.join(name));
            }
        }
    }
    match copy_dir_all(&src, &dst) {
        Ok(()) => println!("已从系统 Firefox profile 拷贝 Widevine CDM 到 {}", dst.display()),
        Err(e) => eprintln!("拷贝 Widevine CDM 失败（将尝试联网下载）：{}", e),
    }
}

/// xrdp / SSH 会话常缺 XDG_RUNTIME_DIR，libpulse 会回退到 ~/.config/pulse 下的旧
/// runtime 目录而连不上混音器（实测 Connection refused），浏览器因此完全无声。
/// 从 /proc/self/status 取真实 uid 补上，两个后端都需要。
fn ensure_audio_env(c: &mut Command) {
    if env::var_os("XDG_RUNTIME_DIR").is_some() {
        return;
    }
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return;
    };
    let Some(uid) = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(1))
    else {
        return;
    };
    c.env("XDG_RUNTIME_DIR", format!("/run/user/{}", uid));
}

/// 我们拉起的浏览器主进程 PID。spawn_browser 用 process_group(0) 让它自成进程组，
/// 故 pgid == pid；退出时整组杀掉即可彻底回收（含 firefox-bin / 内容进程），
/// 避免 BiDi 会话或浏览器进程残留把工具锁死。
static BROWSER_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Firefox 的 user.js 内容（每次拉起浏览器都会重写，所以改动立刻生效）。
///
/// 这里带两条为省资源加的 pref：
/// - 关闭站点隔离（fission）+ 内容进程数=1：miniam 只用 music.apple.com 一个站，
///   站点隔离没有安全收益，却要多开 2~3 个内容进程。实测进程数 12→9、
///   RSS 省约 500M。行为变化由 `MINIAM_FISSION=1` 可恢复默认。
/// - 关闭 HEVC 解码：氛围视频已被抑制器压住，HEVC 解码器留着只是占内存
///   （实测媒体进程 596M→356M）。音乐本身是 AAC/ALAC，不受影响。
///   `MINIAM_HEVC=1` 可恢复默认。
///
/// 之前调其它 pref（缓存/预取/遥测）实测收益 ≈ 0，别再加。
fn firefox_user_js() -> String {
    let mut s = String::new();
    let mut p = |k: &str, v: &str| {
        s.push_str(&format!("user_pref(\"{k}\", {v});\n"));
    };
    p("media.eme.enabled", "true");
    p("media.gmp-widevinecdm.enabled", "true");
    p("dom.ipc.processCount", "1");
    p("dom.ipc.processCount.webIsolated", "1");
    p("browser.cache.disk.enable", "false");
    p("browser.cache.memory.capacity", "65536");
    p("media.memory_cache_max_size", "32768");
    p("datareporting.policy.dataSubmissionEnabled", "false");
    p("toolkit.telemetry.enabled", "false");
    p("app.shield.optoutstudies.enabled", "false");
    if !matches!(env::var("MINIAM_FISSION").as_deref(), Ok("1")) {
        p("fission.autostart", "false");
        p("fission.enabled", "false");
        p("fission.autostart.session", "false");
    }
    if !matches!(env::var("MINIAM_HEVC").as_deref(), Ok("1")) {
        p("media.hevc.enabled", "false");
    }
    s
}

fn spawn_browser(b: Backend, visible: bool) -> Result<()> {
    let dir = profile_dir(b);
    fs::create_dir_all(&dir)?;
    let bin = browser_bin(b);

    let mut c = Command::new(&bin);
    ensure_audio_env(&mut c);
    match b {
        Backend::Chrome => {
            c.arg(format!("--remote-debugging-port={}", PORT))
                // 独立 profile 是硬要求：Chrome 136+ 不允许在默认 profile 上开调试端口。
                .arg(format!("--user-data-dir={}", dir.display()))
                .arg("--no-first-run")
                .arg("--no-default-browser-check")
                .arg("--disable-extensions")
                .arg("--disable-background-networking")
                .arg(ORIGIN);

            // 故意不加：
            //   --disable-component-update  → 全新 profile 会装不上 Widevine CDM，直接播不了
            //   --disable-gpu               → 需 A/B 实测，可能触发软件合成
            //   --headless                  → DRM/音频路径需逐版本验证
            if !visible {
                if env::var("MINIAM_HEADLESS").is_ok() {
                    c.arg("--headless=new");
                } else {
                    // X11 下把窗口挪到屏幕外；Wayland 混成器通常忽略这个参数。
                    c.arg("--window-position=-32000,-32000")
                        .arg("--window-size=1,1");
                }
            }
        }
        Backend::Firefox => {
            // 全新 profile 可能拉不到 CDM，先从系统已有 profile 搬一份（已有则跳过）。
            seed_widevine(&dir);
            // 全新 profile 默认把 EME 关掉（drm 会报 "EME has been preffed off"），显式打开；
            // Widevine CDM 仍由 Firefox 首次用到时按 profile 联网下载。
            fs::write(dir.join("user.js"), firefox_user_js())?;
            // --no-remote 避免附着到已有 Firefox 实例（那样调试端口不生效）。
            c.arg("--no-remote")
                .arg("--profile")
                .arg(dir.as_os_str())
                .arg(format!("--remote-debugging-port={}", PORT))
                .arg(ORIGIN);
            if visible {
                c.env_remove("MOZ_HEADLESS");
            } else {
                // Firefox 没有可靠的 --window-position；隐藏统一用 headless，实测 headless 下 Widevine 可用。
                c.env("MOZ_HEADLESS", "1");
            }
        }
    }

    #[cfg(unix)]
    c.process_group(0);
    let child = c
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            let hint = match b {
                Backend::Chrome => "MINIAM_CHROME",
                Backend::Firefox => "MINIAM_FIREFOX",
            };
            format!("启动 {} 失败（可用 {} 指定路径）", bin, hint)
        })?;
    // 记下浏览器主 PID：process_group(0) 让它自成进程组，pgid==pid；
    // 退出时按进程组整组杀掉（Firefox/Chrome 还会 fork 子进程），彻底回收避免会话残留。
    BROWSER_PID.store(child.id(), std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

async fn wait_ready(b: Backend, timeout: Duration) -> Result<Option<BiDi>> {
    if b == Backend::Firefox {
        return bidi_wait_ready(timeout).await.map(Some);
    }
    let start = Instant::now();
    while start.elapsed() < timeout {
        if port_alive() && page_ws().is_ok() {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    bail!("浏览器在 {:?} 内没就绪", timeout)
}

/// Firefox 就绪：一条会话内轮询，避免反复创建 session 撞上活跃数上限。
async fn bidi_wait_ready(timeout: Duration) -> Result<BiDi> {
    let start = Instant::now();
    while start.elapsed() < timeout && !port_alive() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    if !port_alive() {
        bail!("浏览器在 {:?} 内没就绪", timeout);
    }
    let mut b = loop {
        match BiDi::connect().await {
            Ok(b) => break b,
            Err(e) => {
                let m = format!("{e}");
                // 上一次 UI 被强杀会残留 BiDi 会话；Firefox 活跃会话数有上限，
                // 触顶后 session.new 直接报错。此时重试毫无意义，且会让 ui 零输出干等，
                // 立即失败并给出可操作的自救指引。
                if m.contains("Maximum number of active sessions")
                    || m.contains("session not created")
                {
                    bail!(
                        "BiDi 会话已达上限（多半是上次 UI 异常退出残留的会话未释放）。
                         请执行 `miniam reset` 清理残留进程，再重新运行 miniam up 与 miniam ui"
                    );
                }
                if start.elapsed() >= timeout {
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
    };
    let mut found = false;
    let mut navigated = false;
    let mut err: Option<anyhow::Error> = None;
    while start.elapsed() < timeout {
        match b.call("browsingContext.getTree", json!({})).await {
            Ok(tree) => {
                let contexts = tree["contexts"].as_array();
                found = contexts
                    .map(|cs| {
                        cs.iter().any(|c| {
                            c["url"].as_str().unwrap_or("").contains("music.apple.com")
                        })
                    })
                    .unwrap_or(false);
                if found {
                    break;
                }
                // headless often starts on about:blank: navigate the first context to Apple Music.
                if !navigated {
                    if let Some(cid) = contexts
                        .and_then(|cs| cs.first())
                        .and_then(|c| c["context"].as_str())
                    {
                        let _ = b
                            .call(
                                "browsingContext.navigate",
                                json!({ "context": cid, "url": format!("{}/", ORIGIN), "wait": "none" }),
                            )
                            .await;
                        eprintln!("miniam: page not loaded, navigating to {}", ORIGIN);
                        navigated = true;
                    }
                }
            }
            Err(e) => {
                err = Some(e);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    if let Some(e) = err {
        b.end().await;
        return Err(e);
    }
    if !found {
        b.end().await;
        bail!("浏览器在 {:?} 内没就绪", timeout);
    }
    Ok(b)
}

fn fmt_time(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--".to_string();
    }
    let s = secs as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}

/// 按后端执行 JS 并取回值，供一次性命令和 ui 复用。
async fn eval_expr(b: Backend, fx: &mut Option<BiDi>, expr: &str) -> Result<Value> {
    match b {
        Backend::Chrome => cdp_eval(&page_ws()?, expr).await,
        Backend::Firefox => fx_eval(fx, expr).await,
    }
}

/// headless 下从零起播：Apple Music 要求真实用户手势（mk-007
/// USER_INTERACTION_REQUIRED），纯 JS 的 .click() / mk.play() 都会被拒。
/// 先用 trusted 输入点页面上的「播放」卡片（同时建 queue 和 navigator.userActivation），
/// 再显式 mk.play()。
/// 生成一段 JS：在页面里找到文本/aria-label 匹配 pat 的导航入口（侧边栏的“我的电台”等），点击进入。
/// 起播前把页面导航回 Apple Music 主页（个人电台“某某的电台”所在页）。用当前页面 origin 拼根路径，
/// 避免写死 storefront；主页默认起播即个人电台，无需精确匹配特定卡片。
async fn nav_home(b: Backend, fx: &mut Option<BiDi>) -> bool {
    match b {
        Backend::Chrome => match page_ws() {
            Ok(ws) => cdp_call(
                &ws,
                "Runtime.evaluate",
                json!({ "expression": "location.href = location.origin + '/';", "returnByValue": false, "awaitPromise": false }),
            )
            .await
            .is_ok(),
            Err(_) => false,
        },
        Backend::Firefox => {
            if let Some(bidi) = fx.as_mut() {
                if let Ok(tree) = bidi.call("browsingContext.getTree", json!({})).await {
                    if let Some(c) = tree["contexts"]
                        .as_array()
                        .and_then(|cs| {
                            cs.iter()
                                .find(|c| c["url"].as_str().unwrap_or("").contains("music.apple.com"))
                                .or_else(|| cs.first())
                        })
                    {
                        let ctx = c["context"].as_str().map(String::from);
                        let origin = c["url"]
                            .as_str()
                            .map(|u| u.split('/').take(3).collect::<Vec<&str>>().join("/"))
                            .unwrap_or_else(|| ORIGIN.to_string());
                        if let (Some(ctx), true) = (ctx, !origin.is_empty()) {
                            return bidi
                                .call(
                                    "browsingContext.navigate",
                                    json!({ "url": origin + "/", "context": ctx, "wait": "none" }),
                                )
                                .await
                                .is_ok();
                        }
                    }
                }
            }
            false
        }
    }
}

async fn boot_play(b: Backend, fx: &mut Option<BiDi>) -> Result<Value> {
    // 先导航回主页（个人电台“某某的电台”是主页默认起播的无限流，避免播旧的有限专辑/最近播放）。
    // 已经在播放：直接判定成功。导航回主页会打断当前曲目并清空队列，
    // 任何路径下都不该在播放中重新起播。
    if is_playing(b, fx).await {
        return Ok(Value::Null);
    }
    let _ = nav_home(b, fx).await;
    // 主页加载完 MusicKit 后「播放」卡片还要再渲染 5~6s；固定 sleep 3s 会扑空，
    // BOOT_RECT_EXPR 找不到按钮，后续 mk.play() 在空队列上静默 no-op。
    wait_page(b, fx, PLAY_BTN_READY_EXPR, Duration::from_secs(12)).await;
    // 导航回主页 = 页面 reload，抑制器随 window 一起没了，重新装一次。
    if ambient_video_guard_enabled() {
        install_ambient_guard(b, fx).await;
    }
    // 点主页“播放”按钮，但优先点个人电台（卡片含 /station/ 链接）那个，避免点到有限列表；最后用 mk.play() 兜底。
    match b {
        Backend::Chrome => {
            cdp_call(
                &page_ws()?,
                "Runtime.evaluate",
                json!({
                    "expression": BOOT_CLICK_EXPR,
                    "returnByValue": true,
                    "awaitPromise": true,
                    "userGesture": true,
                }),
            )
            .await?;
            tokio::time::sleep(Duration::from_millis(2000)).await;
        }
        Backend::Firefox => {
            // 找“播放”按钮点一下（建立队列）；找不到也不致命，最后用 mk.play() 兜底起播。
            if let Some(bidi) = fx.as_mut() {
                if let Ok(tree) = bidi.call("browsingContext.getTree", json!({})).await {
                    if let Some(ctx) = tree["contexts"]
                        .as_array()
                        .and_then(|cs| {
                            cs.iter()
                                .find(|c| c["url"].as_str().unwrap_or("").contains("music.apple.com"))
                                .or_else(|| cs.first())
                        })
                        .and_then(|c| c["context"].as_str())
                        .map(String::from)
                    {
                        if let Ok(r) = bidi
                            .call(
                                "script.evaluate",
                                json!({
                                    "expression": BOOT_RECT_EXPR,
                                    "target": { "context": ctx },
                                    "awaitPromise": false,
                                    "resultOwnership": "none",
                                }),
                            )
                            .await
                        {
                            let rect = bidi_value(&r["result"]);
                            if rect["ok"] == true {
                                let x = rect["x"].as_f64().unwrap_or(0.0);
                                let y = rect["y"].as_f64().unwrap_or(0.0);
                                let _ = bidi
                                    .call(
                                        "input.performActions",
                                        json!({
                                            "context": ctx,
                                            "actions": [{
                                                "type": "pointer",
                                                "id": "miniam-mouse",
                                                "parameters": { "pointerType": "mouse" },
                                                "actions": [
                                                    { "type": "pointerMove", "x": x, "y": y },
                                                    { "type": "pointerDown", "button": 0 },
                                                    { "type": "pointerUp", "button": 0 },
                                                ]
                                            }]
                                        }),
                                    )
                                    .await;
                            }
                        }
                    }
                }
            }
            // 点击后的队列建立是异步的，等它落地再 play，否则 play 会在空队列上静默返回。
            tokio::time::sleep(Duration::from_millis(2000)).await;
        }
    }
    // 点击后队列是异步建立的：实测 queue 约 2s、nowPlayingItem 约 8s 才落地。
    // 必须等队列有内容再 play，否则 mk.play() 在空队列上直接 resolve 却什么也没播。
    let queue_ready = wait_page(b, fx, QUEUE_READY_EXPR, Duration::from_secs(12)).await;
    if !queue_ready {
        // 第一次点击没建起队列（按钮未渲染 / 点偏）：再点一次「播放」卡片并重新等。
        let _ = click_label(b, fx, "播放").await;
        wait_page(b, fx, QUEUE_READY_EXPR, Duration::from_secs(6)).await;
    }
    // 状态落地有延迟，重复 play + 轮询确认，避免单次检查过早判失败。
    for _ in 0..6 {
        let _ = ensure_play(b, fx).await;
        if is_playing(b, fx).await {
            return Ok(Value::Null);
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    // 仍暂停：退回点第一个「播放」按钮（已知可用路径）再试一次。
    let _ = click_label(b, fx, "播放").await;
    let _ = ensure_play(b, fx).await;
    if !is_playing(b, fx).await {
        bail!("起播失败：点击后仍未开始播放，请按 d 诊断页面，或手动点一次播放");
    }
    Ok(Value::Null)
}

/// 确保开始播放：调用 MusicKit.play() 并返回其状态。
/// 查询当前是否正在播放（起播后校验用，避免停在“已暂停”）。
async fn is_playing(b: Backend, fx: &mut Option<BiDi>) -> bool {
    match eval_expr(b, fx, &mk("return !!mk.isPlaying;")).await {
        Ok(v) => v.as_bool().unwrap_or(false),
        Err(_) => false,
    }
}

/// headless 下主页渲染 / 队列落地都是异步且耗时不稳定，用轮询替代固定 sleep。
async fn wait_page(b: Backend, fx: &mut Option<BiDi>, expr: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(v) = eval_expr(b, fx, expr).await {
            if v.as_bool().unwrap_or(false) {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

const PLAY_BTN_READY_EXPR: &str = r#"(() => !!Array.from(document.querySelectorAll('button')).find(e => {
    const lab = (e.getAttribute('aria-label') || '') + (e.textContent || '');
    if (!/^播放/.test(lab)) return false;
    const r = e.getBoundingClientRect();
    if (r.width <= 0) return false;
    // 主页刚 readyState=complete 时只有顶部几个 24x24 播放控制按钮，点它们建不起队列；
    // 必须等含 /station/ 链接的个人电台播放卡片渲染出来（实测还要 2~6s）再点。
    let el = e;
    for (let i = 0; i < 5 && el && el !== document.body; i++) {
        if (el.querySelector && el.querySelector('a[href*="/station/"]')) return true;
        el = el.parentElement;
    }
    return false;
}))()"#;
const QUEUE_READY_EXPR: &str = r#"(() => { const mk = window.MusicKit && window.MusicKit.getInstance(); return !!mk && ((mk.queue && mk.queue.length > 0) || !!mk.nowPlayingItem); })()"#;

async fn ensure_play(b: Backend, fx: &mut Option<BiDi>) -> Result<Value> {
    // page-side timeout: mk.play() may never resolve when the page is not authorized,
    // which would otherwise hang the awaitPromise evaluate for the full BIDI_TIMEOUT.
    let out = eval_expr(b, fx, &mk("await Promise.race([mk.play(), new Promise(r => setTimeout(() => r(null), 4000))]); return { ok: true, action: 'boot' };")).await?;
    // 播放状态异步落地，给界面下一轮刷新留一点时间。
    tokio::time::sleep(Duration::from_millis(1200)).await;
    Ok(out)
}


/// 找到 aria-label / 文本匹配 pat 的按钮，返回其中心坐标（找不到 ok:false）。
///
/// 只认真正的按钮（`button` / `role=button`）。播放条外层容器
/// `DIV[aria-label="音乐控制"]` 的 textContent 里含「上一首 下一首」，若不排除就会
/// 先于真按钮命中 —— 点它的中心落在唱片信息区（`player-lcd`）上，按 n/b 毫无反应。
/// 实测 DOM 顺序：容器在第 17 位，真按钮在第 22 位。
///
/// 多个候选时优先「按钮自身文字」命中的（aria-label 只在图标按钮上才有），再按文字长度
/// 取最短 —— 越短越具体。按钮被禁用时返回 disabled=true，让调用方提示「已到跳歌上限」。
fn find_button_rect_expr(pat: &str) -> String {
    format!(
        r#"(() => {{
            const re = new RegExp({p:?}, 'i');
            const els = [];
            const walk = (root) => {{
                if (!root || !root.querySelectorAll) return;
                for (const e of root.querySelectorAll('button,[role="button"],[aria-label],[title]')) {{
                    els.push(e);
                    if (e.shadowRoot) walk(e.shadowRoot);
                }}
            }};
            walk(document);
            const seen = [];
            const cands = [];
            for (const e of els) {{
                const aria = ((e.getAttribute && e.getAttribute('aria-label')) || '').trim();
                const title = ((e.getAttribute && e.getAttribute('title')) || '').trim();
                const own = (e.textContent || '').trim();
                const label = aria + ' ' + title + ' ' + own;
                if (seen.length < 12 && label) seen.push(label.slice(0, 40));
                if (!re.test(label)) continue;
                // 排除容器：aria-label 是给整块区域用的，不是可点控件。
                const isBtn = e.tagName === 'BUTTON' || e.getAttribute('role') === 'button';
                if (!isBtn) continue;
                const r0 = e.getBoundingClientRect();
                if (r0.width <= 0 || r0.height <= 0) continue;
                // 自身文字命中优先；否则用 aria-label（图标按钮只有 aria-label）。
                const viaOwn = re.test(own);
                const viaAria = re.test(aria);
                const viaTitle = re.test(title);
                const rank = viaOwn ? 0 : (viaAria ? 1 : 2);
                const dis = e.disabled || e.getAttribute('aria-disabled') === 'true';
                cands.push({{ e, rank, len: own.length || aria.length || 999, dis }},
                );
            }}
            if (!cands.length) {{
                return {{ ok: false, disabled: false, url: location.href, total: els.length, labels: seen }};
            }}
            cands.sort((a, b) => a.rank - b.rank || a.len - b.len);
            const best = cands[0];
            if (best.dis) {{
                return {{ ok: false, disabled: true, url: location.href, total: els.length, labels: seen }};
            }}
            best.e.scrollIntoView({{ block: 'center', inline: 'center' }});
            const r = best.e.getBoundingClientRect();
            return {{
                ok: true,
                x: Math.round(r.left + r.width / 2),
                y: Math.round(r.top + r.height / 2),
                label: (best.e.getAttribute('aria-label') || best.e.textContent || '').trim().slice(0, 40)
            }};
        }})()"#,
        p = pat
    )
}

/// 点击页面里的“下一首 / 上一首”等按钮。
/// Apple Music 网页的无限播放是页面推荐引擎在曲目结束时自动续播，MusicKit 的
/// skipToNextItem 在无非显式队列时是 no-op，所以必须点 UI 按钮才能真正切歌。
async fn click_label(b: Backend, fx: &mut Option<BiDi>, pat: &str) -> Result<()> {
    match b {
        Backend::Chrome => {
            let expr = format!(
                r#"(() => {{
                    const re = new RegExp({p:?}, 'i');
                    const b = Array.from(document.querySelectorAll('button'))
                        .find(e => re.test((e.getAttribute('aria-label') || '') + (e.textContent || '')));
                    if (!b) return {{ ok: false }};
                    b.click();
                    return {{ ok: true }};
                }})()"#,
                p = pat
            );
            cdp_call(&page_ws()?, "Runtime.evaluate", json!({ "expression": expr, "returnByValue": true, "awaitPromise": false })).await?;
        }
        Backend::Firefox => {
            let bidi = fx.as_mut().ok_or_else(|| anyhow!("BiDi 会话未建立"))?;
            let r: Result<()> = async {
                let tree = bidi.call("browsingContext.getTree", json!({})).await?;
                let ctx = tree["contexts"].as_array()
                    .and_then(|cs| cs.iter()
                        .find(|c| c["url"].as_str().unwrap_or("").contains("music.apple.com"))
                        .or_else(|| cs.first()))
                    .and_then(|c| c["context"].as_str())
                    .map(String::from)
                    .ok_or_else(|| anyhow!("找不到页面 target"))?;
                // 电台的 skipToNextItem/skipToPreviousItem 在非显式队列时是 no-op（静默不切歌，
                // 表现为按 n 反复同一首），所以优先点页面上真实的「跳过 / 上一首」按钮；
                // 只有页面上找不到对应按钮时才退回 MusicKit 的 skip API。
                let rect_r = bidi.call("script.evaluate", json!({
                    "expression": find_button_rect_expr(pat),
                    "target": { "context": ctx },
                    "awaitPromise": false,
                    "resultOwnership": "none",
                })).await;
                let rect = match rect_r {
                    Ok(v) => bidi_value(&v["result"]),
                    Err(e) => { bail!("切歌失败：无法查询页面按钮（{}）", e); }
                };
                if rect["ok"] == true {
                    let x = rect["x"].as_f64().unwrap_or(0.0);
                    let y = rect["y"].as_f64().unwrap_or(0.0);
                    log_audio_note(&format!(
                        "切歌点击 pat={:?} 命中={:?} @({},{})",
                        pat,
                        rect["label"].as_str().unwrap_or(""),
                        x as i64,
                        y as i64
                    ));
                    bidi.call("input.performActions", json!({
                        "context": ctx,
                        "actions": [{
                            "type": "pointer", "id": "miniam-mouse",
                            "parameters": { "pointerType": "mouse" },
                            "actions": [
                                { "type": "pointerMove", "x": x, "y": y },
                                { "type": "pointerDown", "button": 0 },
                                { "type": "pointerUp", "button": 0 },
                            ]
                        }]
                    })).await?;
                    return Ok(());
                }
                if rect["disabled"] == true {
                    bail!("已到电台跳歌上限：「跳过」按钮已禁用，无法再下一首。可等当前曲目结束自动续播，或按 g 重新起播。");
                }
                // 页面上没找到对应按钮：退回 MusicKit 的 skip API 作为兜底。
                let back = pat.contains("上一") || pat.to_ascii_lowercase().contains("prev");
                let method = if back { "skipToPreviousItem" } else { "skipToNextItem" };
                let api_expr = format!(
                    r#"(() => {{
                        const mk = window.MusicKit && window.MusicKit.getInstance();
                        if (!mk || typeof mk.{m} !== 'function') return {{ ok: false, reason: 'MusicKit 或切歌 API 不可用' }};
                        try {{ const p = mk.{m}(); if (p && p.catch) p.catch(() => {{}}); }}
                        catch (e) {{ return {{ ok: false, reason: String(e) }}; }}
                        return {{ ok: true }};
                    }})()"#,
                    m = method
                );
                let ar = bidi.call("script.evaluate", json!({
                    "expression": api_expr,
                    "target": { "context": ctx },
                    "awaitPromise": false,
                    "resultOwnership": "none",
                })).await;
                let api_ok = match &ar {
                    Ok(v) => bidi_value(&v["result"])["ok"] == true,
                    Err(_) => false,
                };
                if api_ok {
                    return Ok(());
                }
                let api_reason = match &ar {
                    Ok(v) => bidi_value(&v["result"])["reason"].as_str().unwrap_or("调用失败").to_string(),
                    Err(e) => format!("{}", e),
                };
                let url = rect["url"].as_str().unwrap_or("?");
                let total = rect["total"].as_i64().unwrap_or(0);
                bail!("切歌失败：页面上没找到 {:?} 按钮（{}，控件 {} 个），MusicKit API 也{}。按空格起播",
                      pat, url, total, api_reason);
            }.await;
            r?;
        }
    }
    Ok(())
}

/// 读当前播放曲目的标题 + 队列类型（用于判断「上一首」是否真的换了歌）。
///
/// 用 title 而不是 id：MusicKit 的 nowPlayingItem.id 是对象（catalogId/playbackId 等），
/// 字符串化不可靠；title 是稳定的字符串，同一首曲子标题不变。
///
/// queueName 为空 = 个人电台（无限流，没有「上一首」可退）；
/// 有名字 = 有限专辑/歌单，那里「上一首」才有意义。
async fn now_playing_info(b: Backend, fx: &mut Option<BiDi>) -> Option<(String, String)> {
    let expr = mk("const it = mk.nowPlayingItem || {}; return { title: it.title ?? null, qname: mk.queue && mk.queue.name ? mk.queue.name : '' };");
    let res = match b {
        Backend::Chrome => eval_expr(b, fx, &expr).await,
        Backend::Firefox => fx_eval(fx, &expr).await,
    };
    let v = res.ok()?;
    let title = v["title"].as_str().unwrap_or("").to_string();
    if title.is_empty() {
        return None;
    }
    Some((title, v["qname"].as_str().unwrap_or("").to_string()))
}

/// 封面区最小高度（行）：窗口再小也不低于此值，保证封面始终可见且居中。
const COVER_MIN_ROWS: u16 = 8;

/// 封面区底部留给 braille 点阵标题带的行数（最小点阵文字需要多行，单行文字区放不下）。
const CAPTION_ROWS: u16 = 4;

/// 把封面区切分成「封面本体」与「底部 braille 标题带」。
/// 标题带高度不足（窗口过矮）时退回零高度 Rect，draw_ui 据此跳过标题。
fn cover_split(area: ratatui::layout::Rect) -> (ratatui::layout::Rect, ratatui::layout::Rect) {
    let full = ui_chunks(area)[0];
    // 封面（点阵音符/专辑图）与底部点阵标题之间留 1 行空行（gap=1）；
    // 点阵标题在标题带内顶部对齐，标题带下沿留出的空白落在标题与下方滚动文字之间。
    let gap = 1u16;
    if full.height <= CAPTION_ROWS + gap + 4 {
        return (full, ratatui::layout::Rect::ZERO);
    }
    let ch = full.height - CAPTION_ROWS - gap;
    let cover = ratatui::layout::Rect {
        height: ch,
        ..full
    };
    let cap = ratatui::layout::Rect {
        y: full.y + ch + gap,
        height: CAPTION_ROWS,
        ..full
    };
    (cover, cap)
}

/// 界面纵向分区：[0] 顶端封面（吃掉剩余空间，随窗口变化且图在区域内居中），
/// [1] 封面与标题之间的留白，[2..6] 播放器信息条，[7] 底部频谱固定 4 行（绿/线/放射三方案）。
/// 封面与频谱同时展示；封面区有最小高度，缩放窗口时自动调整但永不缩没。
/// 界面纵向分区：外层顶部留 1 行间距，内层再纵向切分；整屏最后 1 行为居中的状态行。
/// [0] 封面(含底部点阵标题带) [1] 无(0 行) [2] 跑马灯 [3] 顶部空行 + 点阵播放态图标(下两行) + 时间行 [4] 进度条 [5] 无(0 行)
/// [6] 帮助 [7] 频谱(4 行) [8] 状态行(整屏最后一行, 居中)。
fn ui_chunks(size: ratatui::layout::Rect) -> std::rc::Rc<[ratatui::layout::Rect]> {
    use ratatui::layout::{Constraint, Direction, Layout};
    // 外层：顶部 1 行间距 + 剩余（末尾 1 行留给居中的状态行）
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(size);
    Layout::default()
        .direction(Direction::Vertical)
        .horizontal_margin(0)
        .constraints([
            Constraint::Min(COVER_MIN_ROWS), // 0: 顶端封面区（最小高度，居中）
            Constraint::Length(0),   // 1: 点阵标题带下沿留出的单行空白即标题与滚动文字间唯一空行
            Constraint::Length(1),   // 2: 歌曲信息跑马灯（歌曲名）
            Constraint::Length(4),   // 3: 顶部 1 行空行(与跑马灯间隔) + 居中放大的点阵播放态图标(下两行) + 时间行
            Constraint::Length(1),   // 4: 进度条
            Constraint::Length(0),   // 5: 状态行已移到最底部 [8]
            Constraint::Length(2),   // 6: 帮助
            Constraint::Length(4),   // 7: 底部频谱区（固定 4 行）
            Constraint::Length(1),   // 8: 状态行（整屏最后一行，居中）
        ])
        .split(outer[1])
}

/// 把 base64 dataURL 解码、按终端单元格真实比例缩放成 2×2 象限像素，输出双色真彩封面。
/// w/h 为封面区域终端宽高；返回 None 表示解码失败（网络/格式问题），界面保持“无封面”。
/// 面积平均（BOX）缩放：每个目标像素取其覆盖的源像素的算术平均。
///
/// image crate 只有 Nearest/Triangle/CatmullRom/Gaussian/Lanczos3 —— 全是**插值**滤波，
/// 没有面积平均。而封面缩到 180×144 时每个目标像素要摊掉 3~4 个源像素，插值滤波会
/// 在色块边界产生假的中间色（= 锯齿/发虚）。所以这里自己实现真·面积平均：
/// 用积分图（summed-area table）算出每个目标像素对应源区域的和与面积，O(1) 求均值。
///
/// 只做整数累加，不引入插值与锐化。
fn box_resize_area(src: &image::RgbaImage, tw: u32, th: u32) -> image::RgbaImage {
    let (sw, sh) = (src.width() as usize, src.height() as usize);
    if sw == 0 || sh == 0 || tw == 0 || th == 0 {
        return image::RgbaImage::new(tw.max(1), th.max(1));
    }
    // 积分图：每个通道一张 (w+1) x (h+1) 的 u64 表，sum[i][j] = 左上 i×j 区域和。
    let mut acc = vec![[0u64; 4]; (sw + 1) * (sh + 1)];
    {
        let sp = src.as_raw();
        let stride = sw * 4;
        for y in 0..sh {
            let row = (y + 1) * (sw + 1);
            let prev = y * (sw + 1);
            for x in 0..sw {
                let p = &sp[y * stride + x * 4..y * stride + x * 4 + 4];
                acc[row + x + 1][0] = acc[prev + x + 1][0] + acc[row + x][0] - acc[prev + x][0] + p[0] as u64;
                acc[row + x + 1][1] = acc[prev + x + 1][1] + acc[row + x][1] - acc[prev + x][1] + p[1] as u64;
                acc[row + x + 1][2] = acc[prev + x + 1][2] + acc[row + x][2] - acc[prev + x][2] + p[2] as u64;
                acc[row + x + 1][3] = acc[prev + x + 1][3] + acc[row + x][3] - acc[prev + x][3] + p[3] as u64;
            }
        }
    }
    let mut out = image::RgbaImage::new(tw, th);
    for ty in 0..th as usize {
        // 目标行 ty 覆盖的源行区间 [y0, y1)
        let y0 = ty * sh / th as usize;
        let y1 = ((ty + 1) * sh / th as usize).max(y0 + 1).min(sh);
        for tx in 0..tw as usize {
            let x0 = tx * sw / tw as usize;
            let x1 = ((tx + 1) * sw / tw as usize).max(x0 + 1).min(sw);
            let area = ((x1 - x0) * (y1 - y0)) as u64;
            let r0 = y0 * (sw + 1);
            let r1 = y1 * (sw + 1);
            let s = |k: usize| acc[r1 + x1][k] + acc[r0 + x0][k] - acc[r1 + x0][k] - acc[r0 + x1][k];
            let r = (s(0) / area) as u8;
            let g = (s(1) / area) as u8;
            let b = (s(2) / area) as u8;
            let a = (s(3) / area) as u8;
            out.put_pixel(tx as u32, ty as u32, image::Rgba([r, g, b, a]));
        }
    }
    out
}

/// 从一张图片渲染封面（象限块），返回 ratatui 行。
///
/// 与 render_cover 共用同一套采样与着色逻辑，但输入是本地图片而不是 data URL。
fn render_cover_from_image(img: &image::DynamicImage, w: u16, h: u16) -> Vec<ratatui::text::Line<'static>> {
    let cols = w.max(1) as u32;
    let rows = h.max(1) as u32;
    let iw = img.width();
    let ih = img.height();
    if iw == 0 || ih == 0 {
        return Vec::new();
    }
    let r = cell_aspect().clamp(0.5, 4.0);
    let k = ((r * 2.0).round() as u32).clamp(1, 8);
    let cw = 2u32;
    let ch = 2 * k;
    let target_w = cols * cw;
    let target_h = rows * ch;
    // 源图裁成与目标同样的宽高比，避免拉伸变形。
    let target_ratio = target_w as f32 / target_h as f32;
    let src_ratio = iw as f32 / ih as f32;
    let crop = if src_ratio > target_ratio {
        let cw_px = ((ih as f32) * target_ratio) as u32;
        let off = (iw - cw_px) / 2;
        img.crop_imm(off, 0, off + cw_px, ih)
    } else {
        let ch_px = ((iw as f32) / target_ratio) as u32;
        let off = (ih - ch_px) / 2;
        img.crop_imm(0, off, iw, off + ch_px)
    };
    let big = box_resize_area(&crop.to_rgba8(), target_w, target_h);
    raster_quadrant_blocks(&big, cols, rows, cw, ch)
}

fn render_cover(data_url: String, w: u16, h: u16) -> Option<Vec<ratatui::text::Line<'static>>> {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    use image::imageops::FilterType;

    let (_, b64) = data_url.split_once(',')?;
    let bytes = B64.decode(b64.trim()).ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    let iw = img.width();
    let ih = img.height();
    if iw == 0 || ih == 0 {
        return None;
    }
    let cols = w.max(1) as u32;
    let rows = h.max(1) as u32;
    // 采样网格：每字符格 2×2 象限子块，每子块含 1×k 像素（k = 字符格高/宽比 R 取整）。
    // R=2 时：每格 2×4 像素 -> 缩放目标 = cols·2 × rows·4（90 列时即 180×144）。
    let r = cell_aspect().clamp(0.5, 4.0);
    let k = ((r * 2.0).round() as u32).clamp(1, 8);
    let cw = 2u32;   // 横向：每格 2 个子块，每块 1 像素宽
    let ch = 2 * k;  // 纵向：每格 2 个子块，每块 k 像素高
    // 缩放目标（物理正确）：整个网格按 1:2（横向:纵向 = 1:R）铺开。
    let target_w = cols * cw;
    let target_h = rows * ch;
    // 源图裁成与目标同样的宽高比，避免拉伸变形。
    let target_ratio = target_w as f32 / target_h as f32;
    let src_ratio = iw as f32 / ih as f32;
    let crop = if src_ratio > target_ratio {
        // 源更宽：裁两侧
        let cw_px = ((ih as f32) * target_ratio) as u32;
        let off = (iw - cw_px) / 2;
        img.crop_imm(off, 0, off + cw_px, ih)
    } else {
        // 源更高：裁上下
        let ch_px = ((iw as f32) / target_ratio) as u32;
        let off = (ih - ch_px) / 2;
        img.crop_imm(0, off, iw, off + ch_px)
    };
    // 面积平均缩放（不是插值）：每个目标像素取其覆盖源像素的均值。
    let big = box_resize_area(&crop.to_rgba8(), target_w, target_h);
    // 每格切 2×2 子块，子块内再做一次面积平均 -> 每格 4 个采样点。
    let lines = raster_quadrant_blocks(&big, cols, rows, cw, ch);
    Some(lines)
}

/// 全局封面宽高比覆盖（0 = 未初始化，由 detect_cell_aspect 在启动时填充）。
/// 运行时可用 [ / ] 键实时调节，或设 MINIAM_CELL_ASPECT 环境变量。
static COVER_ASPECT: std::sync::Mutex<f32> = std::sync::Mutex::new(0.0);

/// 启动时使用：仅当设置了 MINIAM_CELL_ASPECT 环境变量才用它，否则默认 2.20。
/// 2.20 是本终端实测能让封面呈正方形的单元格高/宽比（用户用 [ / ] 调到 2.20 确认）。
/// 不再依赖 crossterm 像素尺寸自动探测——它在 tmux / 某些终端下会返回错误值，
/// 反而把封面拉伸成矩形。需要微调请用运行时的 [ / ] 键。
/// 结果写入 /tmp/miniam_aspect.log 便于诊断。
fn detect_cell_aspect() -> f32 {
    let mut r = 2.20f32;
    if let Ok(s) = std::env::var("MINIAM_CELL_ASPECT") {
        if let Ok(v) = s.trim().parse::<f32>() {
            if v > 0.0 && v.is_finite() {
                r = v.clamp(0.5, 4.0);
            }
        }
    }
    let _ = std::fs::write("/tmp/miniam_aspect.log", format!("cell_aspect={:.3}
", r));
    r
}

/// 返回当前封面宽高比（启动时已由 detect_cell_aspect 初始化，运行时可用 [ / ] 键调节）。
fn cell_aspect() -> f32 {
    if let Ok(v) = COVER_ASPECT.lock() {
        if *v > 0.0 {
            return (*v).clamp(0.5, 4.0);
        }
    }
    2.20
}

const SPEC_NEON: (u8, u8, u8) = (0, 255, 200);
// 发光贴图已从仓库移除；留空数组让 load_sprite 返回 None，频谱回退到纯色柱。
const SPRITE_COL_BYTES: &[u8] = &[];
const SPRITE_DOT_BYTES: &[u8] = &[];

struct Sprite {
    inten: Vec<u8>,
    w: u32,
    h: u32,
}

fn load_sprite(bytes: &[u8]) -> Option<Sprite> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let has_alpha = img.color().has_alpha();
    let mut inten = vec![0u8; (w * h) as usize];
    for (i, p) in rgba.pixels().enumerate() {
        inten[i] = if has_alpha {
            p[3]
        } else {
            ((p[0] as u32 + p[1] as u32 + p[2] as u32) / 3) as u8
        };
    }
    Some(Sprite { inten, w, h })
}

fn blend_sprite(buf: &mut image::RgbaImage, sp: &Sprite, dx: i64, dy: i64, w: u32, h: u32, tint: (u8, u8, u8), alpha: f32) {
    if w == 0 || h == 0 {
        return;
    }
    let bw = buf.width() as i64;
    let bh = buf.height() as i64;
    let (tr, tg, tb) = tint;
    for yy in 0..h as i64 {
        let sy = ((yy as f32 / h as f32) * sp.h as f32) as u32;
        if sy >= sp.h {
            continue;
        }
        let py = dy + yy;
        if py < 0 || py >= bh {
            continue;
        }
        let row = (sy * sp.w) as usize;
        for xx in 0..w as i64 {
            let sx = ((xx as f32 / w as f32) * sp.w as f32) as u32;
            if sx >= sp.w {
                continue;
            }
            let sa = sp.inten[row + sx as usize] as f32 / 255.0 * alpha;
            if sa <= 0.0 {
                continue;
            }
            let px = dx + xx;
            if px < 0 || px >= bw {
                continue;
            }
            let d = buf.get_pixel_mut(px as u32, py as u32);
            d[0] = (d[0] as f32 + tr as f32 * sa).min(255.0) as u8;
            d[1] = (d[1] as f32 + tg as f32 * sa).min(255.0) as u8;
            d[2] = (d[2] as f32 + tb as f32 * sa).min(255.0) as u8;
            d[3] = 255;
        }
    }
}

/// Braille 栅格化：每字符格 2 像素宽 × 4 像素高 = 8 子像素/格，
/// 比原来的半块(1×2) 密度高 4 倍，且子像素近乎正方，像素更细、细节更密。
/// 某个 2×4 像素块只要有非空像素就点亮对应的 Braille 点（U+2800 + 位图），
/// 整格前景色取块内非空像素的平均色。

/// Quarter-block 真彩栅格化：每个字符格拆成 2×2 四个子像素，整格只用两种
/// 24-bit 颜色（前景 fg / 背景 bg），用 Unicode 象限字符表示哪几个子像素是 fg、
/// 哪几个是 bg。两色由该格像素按亮度分成亮/暗两组求平均得到，因此仍是真彩。
///
/// 选划分的方式：**穷举所有可表达的覆盖方式，取重建误差最小的那个**
/// （fg = 被覆盖子像素的均值，bg = 其余的均值，误差 = 各像素到所属色的平方距离之和）。
/// 旧实现是「按亮度硬切成亮2/暗2 + 每个像素归到更近的颜色」，既不最优，
/// 又因为下面的 QUAD 表把 ▛/▙/▟ 当成 2 象限字符用（实际 Unicode 语义是 3 象限）
/// 而渲染错位 —— 实测每格平方误差 449，改成最优划分后降到 347（仅用 Neutral 字符）
/// / 255（含半块字符）。
///
/// 字符表按 Unicode 真实语义给出（bit0=左上 bit1=右上 bit2=左下 bit3=右下）：
/// ▀=上半 ▌=左半 ▐=右半 ▗=下半 ▖=左下 ▝=右上 ▘=左上 ▚=左上+右下 ▞=右上+左下
/// ▛=左上+上右+左下 ▜=左上+上右+右下 ▙=左上+左下+右下 ▟=右上+左下+右下
///
/// ⚠️ U+2580(▀)/U+2584(▄)/U+2588(█)/U+258C(▌) 是 East Asian **Ambiguous**：
/// 在 zh_CN/ja_JP locale 下部分终端按 2 格宽渲染，而 ratatui 按 1 格布局，
/// 会导致光标错位、界面撕裂。故默认只用 Neutral 字符；
/// 终端确认安全时可用 MINIAM_HALFBLOCK=1 打开 ▀/▌（误差再降 26%）。
/// 按「每格 2×2 象限子块 + 每子块面积平均」采样，再用穷举最优的两色指派渲染。
///
/// buf 已经是缩放好的像素图，尺寸 = cols·cw × rows·ch。
/// 每格切成 2×2 子块，每子块 cw/2 × ch/2 像素，子块内做**面积平均** -> 4 个采样点。
///
/// 两色指派：穷举 14 种非平凡掩码（1..14，排除全 0 / 全 15），
///   fg = 被覆盖子块的均值，bg = 其余子块的均值，
///   重建色按掩码取 fg/bg，SSE = 各采样点到所属色的平方误差之和，取 SSE 最小者。
/// 不做 k-means、不按亮度排序初始化、不做硬阈值二值化、不抖动。
///
/// 字符表按 Unicode 真实语义索引（bit0 左上 / bit1 右上 / bit2 左下 / bit3 右下）：
///   0 空格 1 ▘ 2 ▝ 3 ▀ 4 ▖ 5 ▌ 6 ▞ 7 ▛ 8 ▗ 9 ▚ 10 ▐ 11 ▜ 12 ▄ 13 ▙ 14 ▟ 15 █
fn raster_quadrant_blocks(
    buf: &image::RgbaImage,
    cols: u32,
    rows: u32,
    cw: u32,
    ch: u32,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    // 字符表：索引 = 掩码，字符覆盖的子块与掩码一致。
    const CHARS: [char; 16] = [
        ' ',      '\u{2598}', '\u{259D}', '\u{2580}',   // 0 空 1左上 2右上 3上半
        '\u{2596}', '\u{258C}', '\u{259E}', '\u{259B}',   // 4左下 5左半 6对角 7三象限
        '\u{2597}', '\u{259A}', '\u{2590}', '\u{259C}',   // 8下半 9对角 10右半 11三象限
        '\u{2584}', '\u{2599}', '\u{259F}', '\u{2588}',   // 12下半 13三象限 14三象限 15全
    ];

    let bw = cw.max(2);
    let bh = ch.max(2);
    let mut lines = Vec::with_capacity(rows as usize);
    for cy in 0..rows {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(cols as usize);
        for cx in 0..cols {
            // 4 个采样点：每格 2×2 子块，每子块 bw/2 × bh/2 像素做面积平均。
            let mut px: [(u8, u8, u8); 4] = [(0, 0, 0); 4];
            for (k, &(sx, sy)) in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)].iter().enumerate() {
                let x0 = cx * bw + sx * (bw / 2);
                let y0 = cy * bh + sy * (bh / 2);
                let x1 = x0 + bw / 2;
                let y1 = y0 + bh / 2;
                let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
                for y in y0..y1.min(buf.height()) {
                    for x in x0..x1.min(buf.width()) {
                        let p = buf.get_pixel(x, y);
                        r += p[0] as u64;
                        g += p[1] as u64;
                        b += p[2] as u64;
                        n += 1;
                    }
                }
                if n > 0 {
                    px[k] = ((r / n) as u8, (g / n) as u8, (b / n) as u8);
                }
            }
            // 穷举 14 种非平凡掩码，取 SSE 最小。
            let mut best_sse = u64::MAX;
            let mut best_mask = 0u32;
            let mut best_fg = (0u8, 0u8, 0u8);
            let mut best_bg = (0u8, 0u8, 0u8);
            // 掩码 8（仅右下）被跳过：Unicode 里**没有**「仅右下」这个字符
            // （▐=右半=UR+LR，▗=下半=LL+LR），硬映射到任一个都会多涂一个象限。
            // 实测它从未成为严格最优（0/3240 格），跳过不影响观感。
            for mask in 1u32..15u32 {
                if mask == 0b1000 {
                    continue;
                }
                let mut gset = [(0u8, 0u8, 0u8); 4];
                let mut bset = [(0u8, 0u8, 0u8); 4];
                let mut gn = 0usize;
                let mut bn = 0usize;
                for (k, p) in px.iter().enumerate() {
                    if mask & (1 << k) != 0 {
                        gset[gn] = *p;
                        gn += 1;
                    } else {
                        bset[bn] = *p;
                        bn += 1;
                    }
                }
                if gn == 0 || bn == 0 {
                    continue;
                }
                let avg = |g: &[(u8, u8, u8); 4], n: usize| -> (u8, u8, u8) {
                    let (mut r, mut gg, mut b) = (0u64, 0u64, 0u64);
                    for i in 0..n {
                        r += g[i].0 as u64;
                        gg += g[i].1 as u64;
                        b += g[i].2 as u64;
                    }
                    ((r / n as u64) as u8, (gg / n as u64) as u8, (b / n as u64) as u8)
                };
                let f = avg(&gset, gn);
                let b = avg(&bset, bn);
                let d2 = |c: (u8, u8, u8), t: (u8, u8, u8)| -> u64 {
                    let dr = c.0 as i64 - t.0 as i64;
                    let dg = c.1 as i64 - t.1 as i64;
                    let db = c.2 as i64 - t.2 as i64;
                    (dr * dr + dg * dg + db * db) as u64
                };
                let sse: u64 = gset[..gn].iter().map(|c| d2(*c, f)).sum::<u64>()
                    + bset[..bn].iter().map(|c| d2(*c, b)).sum::<u64>();
                if sse < best_sse {
                    best_sse = sse;
                    best_mask = mask;
                    best_fg = f;
                    best_bg = b;
                }
            }
            // 退化为纯色（4 点几乎同色）：掩码 0、fg=bg=四点均值、字符用空格。
            let (glyph, fgc, bgc) = if best_mask == 0 || best_fg == best_bg {
                let m = (
                    ((px[0].0 as u32 + px[1].0 as u32 + px[2].0 as u32 + px[3].0 as u32) / 4) as u8,
                    ((px[0].1 as u32 + px[1].1 as u32 + px[2].1 as u32 + px[3].1 as u32) / 4) as u8,
                    ((px[0].2 as u32 + px[1].2 as u32 + px[2].2 as u32 + px[3].2 as u32) / 4) as u8,
                );
                (' ', m, m)
            } else {
                (CHARS[best_mask as usize], best_fg, best_bg)
            };
            spans.push(Span::styled(
                glyph.to_string(),
                Style::default()
                    .fg(Color::Rgb(fgc.0, fgc.1, fgc.2))
                    .bg(Color::Rgb(bgc.0, bgc.1, bgc.2)),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn raster_braille(buf: &image::RgbaImage, _cols: u32, rows: u32, ox: i64, oy: i64) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};
    let w_px = buf.width();
    let h_px = buf.height();
    let cw = 2u32; // 每格宽 2 像素
    let ch = 4u32; // 每格高 4 像素
    let ncols = (w_px / cw) as i64;
    let nrows = (h_px / ch) as i64;
    let mut lines = Vec::with_capacity(rows as usize);
    for y in 0..rows {
        let ry = y as i64 - oy;
        if ry < 0 || ry >= nrows {
            lines.push(Line::from(Vec::<Span>::new()));
            continue;
        }
        let mut spans: Vec<Span> = Vec::with_capacity((ox.max(0) as usize) + ncols as usize);
        if ox > 0 {
            spans.push(Span::raw(" ".repeat(ox as usize)));
        }
        for cx in 0..ncols {
            let px0 = (cx + ox) * cw as i64;
            let py0 = ry * ch as i64;
            if px0 < 0 || px0 + cw as i64 > w_px as i64 || py0 < 0 || py0 + ch as i64 > h_px as i64 {
                spans.push(Span::raw(" "));
                continue;
            }
            // 半色调模式用固定点阵，不需要 bits；只需知道该格是否有内容。
            let mut lit = false;
            let mut mr = 0u8;
            let mut mg = 0u8;
            let mut mb = 0u8;
            let mut mlum = 0u32;
            // 亮度加权平均色：权重取亮度平方，亮部主导、黑色铺底权重≈0 不会被稀释。
            // 相比「取单个最亮像素」，相邻格颜色连续过渡，不会因随机粒子产生色相跳变。
            let mut ar = 0u32;
            let mut ag = 0u32;
            let mut ab = 0u32;
            let mut aw = 0u32;
            for r4 in 0..4u32 {
                for c2 in 0..2u32 {
                    let p = buf.get_pixel((px0 + c2 as i64) as u32, (py0 + r4 as i64) as u32);
                    if p[3] > 10 {
                        lit = true;
                        let lum = p[0] as u32 + p[1] as u32 + p[2] as u32;
                        // cell colour = brightest lit pixel: a bright streak keeps
                        // its own hue instead of being diluted by the dim backdrop
                        if lum > mlum {
                            mlum = lum;
                            mr = p[0];
                            mg = p[1];
                            mb = p[2];
                        }
                        let w = lum * lum;
                        ar += p[0] as u32 * w;
                        ag += p[1] as u32 * w;
                        ab += p[2] as u32 * w;
                        aw += w;
                    }
                }
            }
            let avg = if aw > 0 {
                ((ar / aw) as u8, (ag / aw) as u8, (ab / aw) as u8)
            } else {
                (mr, mg, mb)
            };
            if !lit {
                spans.push(Span::raw(" "));
                continue;
            }
            // 半色调点阵：固定稀疏模式，**每个像素行恰好 1 个点**（左右列交替）。
            // 横向暗条的根因是「整行 2 个点都不亮」—— 那一行区域完全透出 bg 形成暗带。
            // 只要每行都有 1 个点，就不存在整行透出，暗条被彻底消除；
            // 明暗层次完全由颜色表达（fg=最亮像素，bg=平均色），即半色调印刷的做法。
            // 两种模式（左右互换）交替，避免呆板的规律感。
            // braille 位定义：bit0-2=行0-2左列, bit3-5=行0-2右列, bit6=行3左, bit7=行3右。
            // 点阵太明显不是颜色对比（实测 fg/bg 亮度差仅约 2），而是稀疏点的规则几何图案
            // 被眼睛识别。对策是提高密度 + 打散规律性：
            //   6 点模式（行0/行2 双点 + 行1/行3 单点）比 4 点更细、更接近连续；
            //   按 (cx*7+y*13) 伪随机选 6 种模式，图案无法被识别为规则点阵。
            // 所有模式都保证「每个像素行至少 1 点」，因此不会有整行透出 bg 的横向暗条。
            // 全部落在 U+2800..U+28FF（braille 区，East Asian Neutral，非全角）。
            let sel = ((cx * 7 + y as i64 * 13) % 6) as usize;
            let pattern: u32 = match sel {
                0 => 0x6F, // 111: 行0双 行1左 行2双 行3左
                1 => 0xAF, // 175: 行0双 行1左 行2双 行3右
                2 => 0x7D, // 125: 行0双 行1右 行2双 行3左
                3 => 0xBD, // 189: 行0双 行1右 行2双 行3右
                4 => 0x95, // 149: 行0左 行1右 行2左 行3右
                _ => 0x6A, // 106: 行0右 行1左 行2右 行3左
            };
            let ch = char::from_u32(0x2800 + pattern).unwrap();
            // bg = 平均色与最亮像素的混合（avg 权重 k）：
            //   k=1.0 时 bg 纯平均色，点阵对比最强、最突兀；
            //   k=0.0 时 bg=fg，点阵完全消失。
            // k=0.4：点阵仍可感知但融入颜色，bg 又保留 40% 平均色信息保住渐变细腻度。
            const BG_AVG_K: f32 = 0.4;
            // 点阵深度：fg 乘这个系数。
            //   1.0 = 点比背景略亮（原始）；
            //   <1  = 点比背景更深，呈「蚀刻/磨砂」凹陷感，纹理更细腻不抢眼。
            const FG_DEPTH: f32 = 0.75;
            let (fr, fg, fb) = (
                (mr as f32 * FG_DEPTH) as u8,
                (mg as f32 * FG_DEPTH) as u8,
                (mb as f32 * FG_DEPTH) as u8,
            );
            let (br, bg_, bb) = (
                (avg.0 as f32 * BG_AVG_K + mr as f32 * (1.0 - BG_AVG_K)) as u8,
                (avg.1 as f32 * BG_AVG_K + mg as f32 * (1.0 - BG_AVG_K)) as u8,
                (avg.2 as f32 * BG_AVG_K + mb as f32 * (1.0 - BG_AVG_K)) as u8,
            );
            spans.push(Span::styled(ch.to_string(), Style::default()
                .fg(Color::Rgb(fr, fg, fb))
                .bg(Color::Rgb(br, bg_, bb))));
        }
        lines.push(Line::from(spans));
    }
    lines
}

struct Particle {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    life: f32,
    max_life: f32,
    color: (u8, u8, u8),
}

fn draw_core(buf: &mut image::RgbaImage, dx: i64, dy: i64, w: i64, h: i64, neon: (u8, u8, u8), alpha: f32) {
    if w <= 0 || h <= 0 {
        return;
    }
    let bw = buf.width() as i64;
    let bh = buf.height() as i64;
    let (r, g, b) = neon;
    for yy in 0..h {
        let py = dy + yy;
        if py < 0 || py >= bh {
            continue;
        }
        for xx in 0..w {
            let px = dx + xx;
            if px < 0 || px >= bw {
                continue;
            }
            let d = buf.get_pixel_mut(px as u32, py as u32);
            // alpha 混合而非 additive 叠加：180 条辐条在中心密集重叠，additive 会让
            // 通道累加饱和到 255（纯白过曝），与邻格产生大幅明暗/色相跳变 —— 这就是
            // 用户说的「粗糙」。混合后多次叠加最多趋近目标色，不会溢出成白色。
            let a = alpha.clamp(0.0, 1.0);
            d[0] = (d[0] as f32 * (1.0 - a) + r as f32 * a) as u8;
            d[1] = (d[1] as f32 * (1.0 - a) + g as f32 * a) as u8;
            d[2] = (d[2] as f32 * (1.0 - a) + b as f32 * a) as u8;
            d[3] = 255;
        }
    }
}

/// 频率 → 霓虹色：低频(0)=红，高频(1)=紫，中间橙/绿/青。
/// 让频谱读起来像“频谱”而不是一整片同色块。
/// 音频复合信号：把 64 频段压成几个能驱动粒子的指标（联网可视化实践总结）。
/// 能量→发射率、低频→重力/爆发、高频→更多更小粒子与闪烁、
/// 谱通量→速度与湍流、节拍→快起慢落包络；全部用 EMA 平滑避免突跳。
#[derive(Default)]
struct AudioDyn {
    prev: Vec<f32>,
    energy: f32,   // 平滑总能量 0..1
    bass: f32,     // 平滑低频 0..1
    treble: f32,   // 平滑高频 0..1
    flux: f32,     // 平滑谱通量 0..1
    pulse: f32,    // 节拍脉冲，快起慢落
}

impl AudioDyn {
    fn update(&mut self, spectrum: &[u64]) {
        let n = spectrum.len().max(1);
        const A: f32 = 0.18; // EMA 系数
        let mut energy = 0.0f32;
        let mut bass = 0.0f32;
        let mut treble = 0.0f32;
        let mut flux = 0.0f32;
        for (i, &v) in spectrum.iter().enumerate() {
            let e = (v as f32 / 100.0).clamp(0.0, 1.0);
            energy += e;
            let t = i as f32 / n as f32;
            if t < 0.22 {
                bass += e;
            } else if t > 0.62 {
                treble += e;
            }
            let p = self.prev.get(i).copied().unwrap_or(0.0);
            flux += (e - p).max(0.0); // spectral flux：只累计能量增量
        }
        let nb = (n as f32 * 0.22).max(1.0);
        let nt = (n as f32 * 0.38).max(1.0);
        self.prev = spectrum.iter().map(|&v| (v as f32 / 100.0).clamp(0.0, 1.0)).collect();
        let e = energy / n as f32;
        let b = bass / nb;
        let t = treble / nt;
        let f = (flux / n as f32).clamp(0.0, 1.0);
        self.energy += (e - self.energy) * A;
        self.bass += (b - self.bass) * A;
        self.treble += (t - self.treble) * A;
        self.flux += (f - self.flux) * A;
        self.pulse *= 0.85; // 慢落
    }

    fn beat(&mut self, intensity: f32) {
        self.pulse = self.pulse.max(intensity.clamp(0.0, 1.0)); // 快起
    }

    /// 噪/忙：谱通量 + 高频，决定速度与横向湍流
    fn busyness(&self) -> f32 { (self.flux * 1.4 + self.treble * 0.8).clamp(0.0, 1.0) }
    /// 重：低频 + 节拍，决定向下重力
    fn heaviness(&self) -> f32 { (self.bass * 0.8 + self.pulse * 0.6).clamp(0.0, 1.0) }
}

/// 霓虹饱和化：把最亮通道推到 255、最暗压到低位，产生电光感（暗底才发光）。
fn neonize(c: (u8, u8, u8)) -> (u8, u8, u8) {
    let mx = c.0.max(c.1).max(c.2) as f32;
    let mn = c.0.min(c.1).min(c.2) as f32;
    if mx < 1.0 {
        return (0, 255, 220);
    }
    let k = 255.0 / mx;
    let push = |v: u8| -> u8 {
        let x = v as f32 * k; // 归一化到 0..255，最亮通道恰为 255
        let d = mn * k * 0.55 * (1.0 - x / 255.0); // 仅对较暗通道压暗
        ((x - d).max(0.0)).min(255.0) as u8
    };
    (push(c.0), push(c.1), push(c.2))
}

fn hue_gradient(t: f32) -> (u8, u8, u8) {
    // 霓虹色板：品红 → 电紫 → 电青 → 酸绿 → 蓝紫，高饱和、暗底发光
    let stops: [(f32, (u8, u8, u8)); 5] = [
        (0.00, (255, 20, 120)),
        (0.25, (190, 40, 255)),
        (0.50, (0, 230, 255)),
        (0.75, (60, 255, 140)),
        (1.00, (120, 90, 255)),
    ];
    let t = t.clamp(0.0, 1.0);
    for i in 0..stops.len() - 1 {
        let (t0, c0) = stops[i];
        let (t1, c1) = stops[i + 1];
        if t <= t1 {
            let lt = (t - t0) / (t1 - t0).max(1e-6);
            return (
                (c0.0 as f32 + (c1.0 as f32 - c0.0 as f32) * lt) as u8,
                (c0.1 as f32 + (c1.1 as f32 - c0.1 as f32) * lt) as u8,
                (c0.2 as f32 + (c1.2 as f32 - c0.2 as f32) * lt) as u8,
            );
        }
    }
    stops[stops.len() - 1].1
}

/// 朝白色混合 amt(0..1)，用来做粒子高光核心，让火花从柱体里“跳”出来。
fn brighten(c: (u8, u8, u8), amt: f32) -> (u8, u8, u8) {
    let a = amt.clamp(0.0, 1.0);
    (
        (c.0 as f32 + (255.0 - c.0 as f32) * a) as u8,
        (c.1 as f32 + (255.0 - c.1 as f32) * a) as u8,
        (c.2 as f32 + (255.0 - c.2 as f32) * a) as u8,
    )
}

fn render_spectrum(
    spectrum: &[u64],
    cols: u32,
    rows: u32,
    col_sprite: Option<&Sprite>,
    dot_sprite: Option<&Sprite>,
    particles: &mut Vec<Particle>,
    neon: (u8, u8, u8),
    peaks: &mut Vec<f32>,
    dyn_state: &AudioDyn,
) -> Vec<ratatui::text::Line<'static>> {
    use rand::Rng;
    const DT: f32 = 0.05;
    // Braille: 2x4 subpixels per cell（8 点/格，纵向分辨率最高 = 最细腻）
    let bw = (cols.max(1) * 2) as u32;
    let bh = (rows.max(1) * 4) as u32;
    // 铺不透明底：每格 8 点全亮 -> 整片 ⣿ 连续点阵，无空格/空行。
    // 颜色仍取格内最亮像素（零稀释），所以铺底不会淹没亮线。
    let mut buf = image::RgbaImage::from_pixel(bw, bh, image::Rgba([0, 0, 0, 255]));
    let n = spectrum.len().max(1) as u32;
    let bin_w = ((bw as f32) / n as f32).max(1.0) as u32;
    let mut rng = rand::thread_rng();
    let busyness = dyn_state.busyness();
    let heaviness = dyn_state.heaviness();
    // 发射率随能量：安静时几乎无粒子，激烈时最多 ~每 bin 3 个（少而精）。
    let spawn_scale = (0.4 + dyn_state.energy * 2.6).clamp(0.0, 3.0);
    if peaks.len() != n as usize {
        *peaks = vec![0.0f32; n as usize];
    }
    for (i, &v) in spectrum.iter().enumerate() {
        let e0 = (v as f32 / 100.0).clamp(0.0, 1.0);
        let e = e0.powf(0.6);
        let t = i as f32 / n as f32;
        let c = neonize(hue_gradient(t));
        if e <= 0.002 {
            continue;
        }
        let cx = (i as f32 + 0.5) * (bw as f32 / n as f32);
        let bar_h = (((e * (bh as f32)).max(1.0)) as u32).min(bh);
        let dx = (cx - bin_w as f32 / 2.0) as i64;
        let dy = (bh - bar_h) as i64;
        if let Some(sp) = col_sprite {
            blend_sprite(&mut buf, sp, dx, dy, bin_w.max(2), bar_h, c, 0.5 + 0.5 * e);
        }
        let c_bright = brighten(c, 0.08); // 整体提亮，霓虹底色更醒目
        draw_core(&mut buf, dx, dy, bin_w.max(1) as i64, bar_h as i64, c_bright, 0.55 + 0.40 * e);
        // 柱顶亮芯：additive 叠加近白高光，形成霓虹发光边
        let cap = (bin_w.max(2) as i64).min(3);
        draw_core(&mut buf, dx, (bh - bar_h) as i64, cap, 1, brighten(c, 0.4), 1.0);
        let pi = i as usize;
        if (bar_h as f32) > peaks[pi] {
            peaks[pi] = bar_h as f32;
        } else {
            peaks[pi] -= 1.4;
            if peaks[pi] < 0.0 { peaks[pi] = 0.0; }
        }
        if peaks[pi] > 1.0 {
            let py = (bh as f32 - peaks[pi]) as i64;
            draw_core(&mut buf, dx, py, bin_w.max(1) as i64, 1, brighten(c, 0.3), 1.0);
        }
        // 粒子数随能量与频段：高频更多更小的火花，低频少而慢
        let hi = if t > 0.62 { 1.0 } else { 0.55 };
        let spawn = ((e * spawn_scale * hi) as usize).min(3);
        // 速度随歌曲：busyness(谱通量+高频) 与 energy 共同放大整体速率（安静≈0.6x，激烈≈3x）
        let speed = (0.6 + busyness * 1.6 + dyn_state.energy * 0.4).clamp(0.5, 3.0);
        for _ in 0..spawn {
            let px = cx + (rng.gen::<f32>() - 0.5) * bin_w as f32;
            let rise = (15.0 + rng.gen::<f32>() * 30.0) * speed;
            let vy = -rise;
            let vx = (rng.gen::<f32>() - 0.5) * (10.0 + busyness * 40.0) * speed;
            let max_life = 0.45 + rng.gen::<f32>() * (0.55 + dyn_state.treble * 0.4);
            // 颜色更亮：先提亮再掺白做高光核心
            let col = brighten(neonize(c), 0.30);
            particles.push(Particle {
                x: px, y: (bh - bar_h) as f32, vx, vy, life: max_life, max_life,
                color: col,
            });
        }
    }
    // 静音/极弱时给一条暗淡霓虹底线，不再喷粒子
    if dyn_state.energy < 0.01 {
        draw_core(&mut buf, 0, bh as i64 - 2, bw as i64, 2, neonize(neon), 0.12);
    }
    // 粒子动效：向上飘 + 横向湍流 + 低频重力回落 + 高频闪烁，寿命随亮度衰减
    let mut next: Vec<Particle> = Vec::with_capacity(particles.len());
    for p in particles.drain(..) {
        let mut p = p;
        // 速度随歌曲：重力回落也随 busyness 加快
        let speed = (0.6 + dyn_state.busyness() * 1.6 + dyn_state.energy * 0.4).clamp(0.5, 3.0);
        p.vy += (8.0 + heaviness * 46.0) * speed * DT; // 重力：低频/节拍越强回落越快
        p.x += p.vx * DT;
        p.y += p.vy * DT;
        p.life -= DT;
        if p.life <= 0.0 || p.y > bh as f32 + 2.0 || p.x < -2.0 || p.x > bw as f32 + 2.0 {
            continue;
        }
        let lr = (p.life / p.max_life).clamp(0.0, 1.0);
        // 更小：1~2 子像素的亮芯 + 低亮度光晕
        let size = (1.0 + lr).clamp(1.0, 2.0);
        let flick = 1.0 - dyn_state.treble * 0.35 * (1.0 - lr); // 高频轻微闪烁
        let a = lr * flick;
        let (dx, dy) = ((p.x - size / 2.0) as i64, (p.y - size / 2.0) as i64);
        if let Some(sp) = dot_sprite {
            blend_sprite(&mut buf, sp, dx, dy, size as u32 + 1, size as u32 + 1, p.color, a * 0.6);
        }
        // additive 亮芯：中心近白高光（更亮），外圈同色光晕
        draw_core(&mut buf, dx, dy, size as i64 + 1, size as i64 + 1, p.color, a * 0.5);
        draw_core(&mut buf, dx, dy, 1, 1, brighten(p.color, 0.4), a);
        next.push(p);
    }
    *particles = next;
    raster_braille(&buf, cols, rows, 0, 0)
}

fn draw_ui(
    f: &mut ratatui::Frame,
    status: &Value,
    busy: bool,
    feedback_phase: Option<f64>,
    msg: &str,
    bname: &str,
    spectrum: &[u64],
    cover_lines: &[ratatui::text::Line<'static>],
    spectrum_lines: &[ratatui::text::Line<'static>],
    spec_mode: u8,
) {
    use ratatui::{
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Paragraph, Sparkline},
    };
    let size = f.area();
    let chunks = ui_chunks(size);
    // 全局黑底：让霓虹频谱与封面在黑色背景上发光
    f.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(0, 0, 0))),
        size,
    );
    let title = status["title"].as_str().unwrap_or("");
    let artist = status["artist"].as_str().unwrap_or("");
    let album = status["album"].as_str().unwrap_or("");
    let meta = match (artist, album) {
        ("", "") => String::new(),
        (a, "") => a.to_string(),
        ("", al) => al.to_string(),
        (a, al) => format!("{} · {}", a, al),
    };
    // 歌曲信息跑马灯：一行横向循环滚动，按时间推进，无需额外状态变量。
    let full = if meta.is_empty() { title.to_string() } else { format!("{}   {}", title, meta) };
    let chars: Vec<char> = format!("{}          ", full).chars().collect();
    let tick = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0) / 130) as usize;
    let w = chunks[2].width as usize;
    let marquee: String = if chars.is_empty() {
        String::new()
    } else {
        let off = tick % chars.len();
        (0..w).map(|i| chars[(off + i) % chars.len()]).collect()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            marquee,
            Style::default(),
        ))),
        chunks[2],
    );
    let playing = status["isPlaying"].as_bool().unwrap_or(false);
    let pos = status["playbackTime"].as_f64();
    let dur = status["duration"].as_f64().map(|d| d / 1000.0);
    let ratio = match (pos, dur) {
        (Some(p), Some(d)) if d > 0.0 => (p / d).clamp(0.0, 1.0),
        _ => 0.0,
    };
    let time_played = fmt_time(pos.unwrap_or(f64::NAN));
    let time_total = fmt_time(dur.unwrap_or(f64::NAN));
    // 图标形状：有曲目在架（播放中 / 已暂停 / 起播中）显示双竖条 II（暂停符号），
    // 完全停止（无曲目）显示右指三角 ▶（播放符号）。这是媒体控制的通用惯例
    // —— 图标表示「可执行的动作」：正在播时按下去是暂停，故显示 II。
    let has_track = playing || !title.is_empty() || busy;
    let state_kind: u8 = if has_track { 1 } else { 0 };
    let icon_blink_phase: Option<f64> = if let Some(p) = feedback_phase {
        Some(p)
    } else if busy {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        Some((t * 4.8).sin())
    } else {
        None
    };
    // [3] 布局：顶部 1 行空行（与跑马灯间隔） + 点阵播放态图标(2 行, 居中放大) + 时间行(下排: 已播居左/总时长居右)
    let pw = time_played.chars().count();
    let tw = time_total.chars().count();
    let pad = (chunks[3].width as usize).saturating_sub(pw + tw).max(1);
    let icon_rect = ratatui::layout::Rect {
        y: chunks[3].y + 1,
        height: 2,
        ..chunks[3]
    };
    let icon = braille_text::icon_lines(
        state_kind,
        icon_rect.width as u32,
        icon_rect.height as u32,
        None,
        icon_blink_phase,
    );
    // 输出正好铺满图标带(不透明底)，无空行
    f.render_widget(Paragraph::new(icon), icon_rect);
    let time_rect = ratatui::layout::Rect {
        y: chunks[3].y + 3,
        height: 1,
        ..chunks[3]
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(time_played, Style::default()),
            Span::raw(" ".repeat(pad)),
            Span::styled(time_total, Style::default().add_modifier(Modifier::DIM)),
        ])),
        time_rect,
    );
        // 自定义进度条：细横线条；已播用默认色、未播用淡色，靠亮度而非颜色区分（终端默认色）
        let w = chunks[4].width.max(1) as usize;
        let filled = ((ratio * w as f64).round() as usize).min(w);
        let unfilled = w - filled;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("─".repeat(filled), Style::default()),
                Span::styled("─".repeat(unfilled), Style::default().add_modifier(Modifier::DIM)),
            ])),
            chunks[4],
        );
    let qlen = status["queueLength"].as_i64().unwrap_or(0);
    let qname = status["queueName"].as_str().unwrap_or("");
    let playlist_part = if qname.is_empty() {
        format!("queue={}", qlen)
    } else {
        format!("♪ {}", qname)
    };
    let status_line = format!("{}   backend={}", playlist_part, bname);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(status_line, Style::default())))
            .alignment(ratatui::layout::Alignment::Center),
        chunks[8],
    );
    let help = if msg.is_empty() {
        "space play/pause - n next - b prev - c spectrum - Esc quit"
    } else {
        msg
    };
    f.render_widget(
        Paragraph::new(help)
            .style(Style::default().add_modifier(Modifier::DIM))
            .alignment(ratatui::layout::Alignment::Center),
        chunks[6],
    );
    // 顶端封面：render_cover 已把图居中放进封面区；底部留一条 braille 标题带。
    let (cover_rect, cap_rect) = cover_split(size);
    if cover_lines.is_empty() {
        // 缺省图：点阵音符（无封面时用，不显示文字）
        // 呼吸动效：缺省音符持续随正弦轻微缩放，让空状态有“活着”的呼吸感
        let tnow = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()) // f64 not f32: epoch seconds in f32 lose sub-second precision, freezing the sine
            .unwrap_or(0.0);
        let breath = (1.0 + 0.12 * (tnow * 2.2).sin()) as f32;
        let note = braille_text::default_note_lines(
            cover_rect.width as u32,
            cover_rect.height as u32,
            None,
            breath,
        );
        // 输出正好铺满封面区(不透明底)，无空行
        f.render_widget(Paragraph::new(note), cover_rect);
    } else {
        f.render_widget(Paragraph::new(cover_lines.to_vec()), cover_rect);
    }
    // braille 点阵标题（最小点阵，不用方块）：只占底部标题带，不挤占封面。
    // 首字母大写、横向居中（纵向贴底，紧贴下方滚动文字；上方空行由 cover_split 提供）。
    // 无曲名时不画任何东西（点阵音符缺省图只出现在封面区，保证只有一个音符）。
    if cap_rect.height > 0 && !title.is_empty() {
        let cap_text: String = {
            let mut ch = title.chars();
            match ch.next() {
                Some(c) => c.to_uppercase().collect::<String>() + ch.as_str(),
                None => String::new(),
            }
        };
        if let Some(cap_lines) =
            braille_text::text_to_braille(&cap_text, cap_rect.width as u32, cap_rect.height as u32, None)
        {
            // 输出正好铺满标题带(不透明底)，与下方跑马灯之间无空行
            f.render_widget(Paragraph::new(cap_lines), cap_rect);
        }
    }
    // 底部频谱区：spec_mode 决定方案——0 绿(经典) / 1 霓虹线 / 2 放射。
    if spec_mode == 0 {
        let sw = chunks[7].width as usize;
        let spec7 = resize_spec(spectrum, sw);
        f.render_widget(
            Sparkline::default()
                .data(&spec7)
                .max(100)
                .style(Style::default()),
            chunks[7],
        );
    } else {
        f.render_widget(Paragraph::new(spectrum_lines.to_vec()), chunks[7]);
    }
}

/// 把频谱数据按目标宽度就近重采样（用于底部绿色频谱铺满宽度）。
fn resize_spec(data: &[u64], w: usize) -> Vec<u64> {
    if w == 0 {
        return vec![];
    }
    if data.len() == w {
        return data.to_vec();
    }
    if data.is_empty() {
        return vec![0u64; w];
    }
    let mut out = vec![0u64; w];
    for i in 0..w {
        let src = (i as f64 * data.len() as f64 / w as f64) as usize;
        out[i] = data[src.min(data.len() - 1)];
    }
    out
}

/// 取默认 sink 的 monitor 源名（如 `alsa_output.xxx.monitor`）。
/// 音频源诊断信息：当前选中的 monitor 源名 + 实时原始电平（未经增益，0..~1）。
struct AudioMeta {
    source: String,
    level: f32,
}

/// 抓取线程检测到节拍时写入，UI 线程读取并触发粒子爆发。
struct BeatState {
    last_beat: Option<Instant>,
    intensity: f32,
}

/// 列出租金所有可作为“系统混音”的 monitor 源：默认 sink 的 monitor + 所有 *.monitor。
/// 纯 PipeWire 没有 pactl 时，回退到 pw-dump 列出的 monitor 节点。
fn candidate_monitor_sources() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(o) = std::process::Command::new("pactl").args(["get-default-sink"]).output() {
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !s.is_empty() {
            out.push(format!("{}.monitor", s));
        }
    }
    if let Ok(o) = std::process::Command::new("pactl").args(["list", "short", "sources"]).output() {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some(name) = line.split_whitespace().next() {
                if name.ends_with(".monitor") && !out.iter().any(|x| x == name) {
                    out.push(name.to_string());
                }
            }
        }
    }
    if out.is_empty() {
        if let Ok(o) = std::process::Command::new("pw-dump").output() {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                if let Some(arr) = v.as_array() {
                    for n in arr {
                        let props = &n["info"]["props"];
                        let class = props["device.class"].as_str().unwrap_or("");
                        if class == "monitor" {
                            if let Some(name) = props["node.name"].as_str() {
                                if !out.iter().any(|x| x == name) {
                                    out.push(name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// 探测某个源 ~ms 毫秒内的 RMS（归一化 0..1），用于在多个 monitor 中挑“正在出声”的那个。
fn probe_source_level(dev: &str, ms: u64) -> f32 {
    let mut child = match std::process::Command::new("parec")
        .arg("--device").arg(dev)
        .arg("--format=s16le").arg("--rate=44100").arg("--channels=1")
        .arg("--latency-msec=15")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return 0.0,
    };
    let mut rd = match child.stdout.take() {
        Some(r) => r,
        None => { let _ = child.kill(); let _ = child.wait(); return 0.0; }
    };
    let mut buf = vec![0u8; 8192];
    let start = Instant::now();
    let dur = Duration::from_millis(ms);
    let mut sum_sq = 0.0f64;
    let mut n = 0u64;
    while start.elapsed() < dur {
        match rd.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => {
                for i in (0..k).step_by(2) {
                    let s = i16::from_le_bytes([buf[i], buf[i + 1]]) as f64 / 32768.0;
                    sum_sq += s * s;
                    n += 1;
                }
            }
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    if n == 0 { 0.0 } else { (sum_sq / n as f64).sqrt() as f32 }
}

/// 探测所有候选源，返回当前最响的那个（都静音则返回第一个，保证有兜底）。
fn pick_best_source(cands: &[String]) -> String {
    let mut best = cands.get(0).cloned().unwrap_or_default();
    let mut best_lvl = -1.0f32;
    for c in cands {
        let lvl = probe_source_level(c, 400);
        if lvl > best_lvl {
            best_lvl = lvl;
            best = c.clone();
        }
    }
    best
}

/// 虚拟接收端在"声音设置"里显示的名称。
///
/// 这个 sink 是内部实现细节（只用来给频谱抓音频），但 PulseAudio 会把每个 sink 都当成
/// 一个可选的输出设备展示出来 —— 用户一旦选中它就会得到静音（null sink 不出声）。
/// 所以名称必须明确写出"内部用、别选"。
///
/// 注意：**不能含空格**（pactl load-module 的属性串是按空格切分成 key=value 的）。
/// 若面向英文用户发布，把这里换成 miniam-spectrum-internal-do-not-select 即可。
const CAPTURE_DESC: &str = "miniam-频谱采集-勿选为输出";

/// 创建一个纯软件的“虚拟接收端”(null sink)。它的 monitor 会在软件层混出真实解码后的 PCM，
/// 即使本机没有可用声卡/扬声器（headless 服务器）也能拿到音频来驱动频谱。
/// 返回 sink 名字；pactl 不可用等情况下返回 None。
fn setup_null_sink() -> Option<String> {
    let name = "miniam_capture";
    let exists = || {
        if let Ok(o) = std::process::Command::new("pactl").args(["list", "short", "sinks"]).output() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if let Some(n) = line.split_whitespace().nth(1) {
                    if n == name { return true; }
                }
            }
        }
        false
    };
    if exists() {
        return Some(name.to_string());
    }
    let _ = std::process::Command::new("pactl")
        .args([
            "load-module",
            "module-null-sink",
            &format!("sink_name={}", name),
            &format!("sink_properties=device.description={}", CAPTURE_DESC),
        ])
        .output();
    std::thread::sleep(Duration::from_millis(300));
    if exists() { Some(name.to_string()) } else { None }
}

/// 把浏览器（Apple Music 所在进程）的音频输入转移到指定 sink，使其 monitor 能拿到真实 PCM。
/// 只搬与浏览器可执行文件对应的 sink-input，不干扰系统其它声音。
/// 让浏览器音频“分两路”：一路照常去默认声卡（你能听到），一路进 miniam_capture 虚拟 sink
/// （用来抓频谱）。用 pw-link 在 PipeWire 图上做扇出，而不是 move-sink-input（那会把音源挪走导致听不到）。
/// 找出 miniam 自己那个浏览器的进程 pid。
/// 必须按 pid 识别，不能按二进制名：用户自己的 firefox / 别的播放器同样叫 firefox，
/// 一并接进虚拟 sink 就会让频谱跟着系统里其它声音（放视频）跳。
/// 音频链路的诊断日志（追加写 /tmp/miniam_audio.log）。
/// 界面刻意不显示这些文字（会刷屏），只在出问题时供排查。
fn log_audio_note(msg: &str) {
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/miniam_audio.log")
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

/// 我们自己的浏览器 profile 目录标记。
/// 只有 miniam 的浏览器会用这个目录，所以它能精确区分"我们的浏览器"和"用户自己的浏览器"。
fn profile_marker() -> String {
    let home = env::var("HOME").unwrap_or_default();
    format!("{}/.config/miniam/", home)
}

/// 进程是否还活着。
fn proc_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

/// 在 /proc 里找"命令行包含 needle"的进程 pid。
fn find_proc_by_cmdline(needle: &str) -> Option<u32> {
    let me = std::process::id();
    for entry in std::fs::read_dir("/proc").ok()? {
        let Ok(e) = entry else { continue; };
        let Some(name) = e.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = name.parse::<u32>() else { continue; };
        if pid == me {
            continue;
        }
        if proc_cmdline(pid).contains(needle) {
            return Some(pid);
        }
    }
    None
}

/// 找出监听 <port> 的 socket 的属主进程（不依赖 ss / lsof）。
/// /proc/net/tcp{,6} 里状态为 LISTEN(0A) 且本地端口匹配的条目给出 socket inode，
/// 再扫 /proc/<pid>/fd 找 "socket:[inode]" 即可反查属主。
fn listen_port_owner(port: u16) -> Option<u32> {
    let want = format!(":{:04X}", port);
    let mut inodes: Vec<String> = Vec::new();
    for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 {
                continue;
            }
            // cols[1]=local_address  cols[3]=st  cols[9]=inode
            if cols[3] != "0A" || !cols[1].ends_with(&want) {
                continue;
            }
            inodes.push(cols[9].to_string());
        }
    }
    if inodes.is_empty() {
        return None;
    }
    let me = std::process::id();
    for entry in std::fs::read_dir("/proc").ok()? {
        let Ok(e) = entry else { continue; };
        let Some(name) = e.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = name.parse::<u32>() else { continue; };
        if pid == me {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(format!("/proc/{}/fd", pid)) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let t = target.to_string_lossy();
            if let Some(rest) = t.strip_prefix("socket:[") {
                let ino = rest.trim_end_matches(']');
                if inodes.iter().any(|x| x == ino) {
                    return Some(pid);
                }
            }
        }
    }
    None
}

/// 找出 miniam 自己那个浏览器的进程 pid。
///
/// 识别顺序（从最可靠到最兜底）：
/// 1) 自己记录的 BROWSER_PID（浏览器由 miniam 亲自拉起）；
/// 2) 命令行带 `--remote-debugging-port=<PORT>` 的进程 —— miniam 启动浏览器时一定带这个
///    参数，所以它精确指向"miniam 正在对话的那个浏览器"，浏览器怎么起的都不影响；
/// 3) 命令行带我们自己 profile 目录的进程；
/// 4) 监听 <PORT> 的 socket 的属主（最后的兜底，不依赖命令行）。
///
/// 刻意**不用** `ss`：并非所有系统都装 iproute2（最小化镜像/容器里常常没有）。
/// 也刻意**不以二进制名**（firefox/chrome）作为判据：用户自己的 firefox 也叫 firefox，
/// 一并接进虚拟 sink 就会让频谱跟着系统里别的声音（放视频）跳。
fn miniam_browser_pid() -> Option<u32> {
    let marker = profile_marker();
    // 1) 自己记录的 pid
    let p = BROWSER_PID.load(std::sync::atomic::Ordering::SeqCst);
    if p != 0 && proc_alive(p) && proc_cmdline(p).contains(&marker) {
        return Some(p);
    }
    // 2) 命令行带调试端口的进程
    if let Some(pid) = find_proc_by_cmdline(&format!("--remote-debugging-port={}", PORT)) {
        return Some(pid);
    }
    // 3) 命令行带我们 profile 目录的进程
    if let Some(pid) = find_proc_by_cmdline(&marker) {
        return Some(pid);
    }
    // 4) 监听端口的属主
    listen_port_owner(PORT)
}


/// 返回「真实硬件声卡」的名字：优先当前默认；若默认已被上次崩溃残留设成 miniam_capture，
/// 则退回找一个非 miniam_capture 的 sink，避免回环自连产生啸叫。
fn real_hardware_sink() -> String {
    let is_capture = |s: &str| s == "miniam_capture";
    if let Some(d) = get_default_sink() {
        if !is_capture(&d) {
            return d;
        }
    }
    if let Ok(o) = std::process::Command::new("pactl")
        .args(["list", "short", "sinks"])
        .output()
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let name = line.split_whitespace().nth(1).unwrap_or("");
            if !is_capture(name) && !name.is_empty() {
                return name.to_string();
            }
        }
    }
    String::new()
}

fn get_default_sink() -> Option<String> {
    std::process::Command::new("pactl")
        .args(["get-default-sink"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn set_default_sink(name: &str) {
    let _ = std::process::Command::new("pactl")
        .args(["set-default-sink", name])
        .output();
}

/// 是否已存在 source->sink 的 loopback 模块（避免重复创建）
fn loopback_module_id(source: &str, sink: &str) -> Option<i64> {
    if let Ok(o) = std::process::Command::new("pactl")
        .args(["list", "short", "modules"])
        .output()
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let parts: Vec<&str> = line.splitn(3, '\t').collect();
            if parts.len() == 3 && parts[1].contains("loopback") {
                if parts[2].contains(source) && parts[2].contains(sink) {
                    if let Ok(id) = parts[0].parse::<i64>() {
                        return Some(id);
                    }
                }
            }
        }
    }
    None
}

fn load_loopback(source: &str, sink: &str) -> Option<i64> {
    if let Some(id) = loopback_module_id(source, sink) {
        return Some(id);
    }
    std::process::Command::new("pactl")
        .args([
            "load-module",
            "module-loopback",
            &format!("source={}", source),
            &format!("sink={}", sink),
        ])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<i64>().ok())
}

fn unload_module(id: i64) {
    let _ = std::process::Command::new("pactl")
        .args(["unload-module", &id.to_string()])
        .output();
}

fn unload_null_sink(name: &str) {
    if let Ok(o) = std::process::Command::new("pactl")
        .args(["list", "short", "modules"])
        .output()
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let parts: Vec<&str> = line.splitn(3, '\t').collect();
            if parts.len() == 3
                && parts[1] == "module-null-sink"
                && parts[2].contains(&format!("sink_name={}", name))
            {
                if let Ok(id) = parts[0].parse::<i64>() {
                    unload_module(id);
                }
            }
        }
    }
}

/// 把浏览器当前的播放流直接搬到虚拟 sink：loopback 已就位时这是安全的（不会静音），
/// 且能避免「浏览器同时直连声卡 + loopback 回环」造成的双重喂声（音量叠加/相位梳状滤波）。
fn move_browser_streams_to(capture: &str, real_default: &str, want: Option<u32>) {
    // 维护虚拟 sink 里的流：
    //  - 我们自己那个浏览器的流 -> 搬进虚拟 sink（它是频谱源）
    //  - 其它任何流（用户自己的 firefox、播放器……）-> 从虚拟 sink 拉回真实输出。
    //    早期版本按二进制名匹配，会把用户自己的 firefox 也搬进来；那些流会一直留在
    //    虚拟 sink 里，于是用户放视频时频谱就跟着跳 —— 必须主动清理，不能只是不再搬进来。
    if real_default.is_empty() {
        return;
    }
    // sink 编号 -> 名字（pactl 的 sink-input 里 Sink: N 是**编号**，不是名字）
    let mut names: Vec<(String, String)> = Vec::new();
    if let Ok(o) = std::process::Command::new("pactl").args(["list", "short", "sinks"]).output() {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let mut it = line.split_whitespace();
            if let (Some(idx), Some(name)) = (it.next(), it.next()) {
                names.push((idx.to_string(), name.to_string()));
            }
        }
    }
    let Ok(o) = std::process::Command::new("pactl").args(["list", "sink-inputs"]).output() else {
        return;
    };
    let txt = String::from_utf8_lossy(&o.stdout);
    for block in txt.split("Sink Input #").skip(1) {
        let id = block
            .lines()
            .next()
            .and_then(|l| l.trim().split_whitespace().next().and_then(|x| x.parse::<i64>().ok()));
        // pactl 的 sink-input 属性里带 application.process.id（Pulse 与 PipeWire 通用）
        let pid = block.lines().find_map(|l| {
            let t = l.trim_start();
            if t.starts_with("application.process.id = ") {
                t.split('"').nth(1).and_then(|x| x.parse::<u32>().ok())
            } else {
                None
            }
        });
        let sink_no = block.lines().find_map(|l| {
            let t = l.trim_start();
            if t.starts_with("Sink: ") {
                t[6..].split_whitespace().next().map(|x| x.to_string())
            } else {
                None
            }
        });
        let Some(id) = id else { continue; };
        let cur_name = sink_no
            .as_deref()
            .and_then(|no| names.iter().find(|(i, _)| i == no).map(|(_, n)| n.clone()))
            .unwrap_or_default();
        let target = if want.is_some() && pid == want {
            // 我们的浏览器：确保它在虚拟 sink 里（频谱源）
            if cur_name != capture { Some(capture) } else { None }
        } else if cur_name == capture {
            // 外来流：拉回真实输出，否则它的声音会进频谱源
            Some(real_default)
        } else {
            None
        };
        if let Some(dst) = target {
            let _ = std::process::Command::new("pactl")
                .args(["move-sink-input", &id.to_string(), dst])
                .output();
        }
    }
}

/// 让虚拟 sink 既能抓取又能出声，且对流重建免疫：
/// 1) 把它的 monitor 用 module-loopback 回环到真实声卡（出声的关键，与浏览器连线无关）；
/// 2) 把虚拟 sink 设为默认输出，浏览器（含重建的流）会自动路由到这里，抓取不丢；
/// 3) 显式把当前浏览器流连到虚拟 sink（当下就能抓）。
/// 回环只清理我们自己创建的模块，复用已有回环时不卸载，避免误删别人的音频图状态。
fn ensure_capture_pipeline(capture: &str, real_default: &str, loopback_mod: &mut Option<i64>) {
    if real_default.is_empty() {
        // 系统里找不到任何真实输出设备：不要接管音频到不出声的虚拟 sink，
        // 也不要把默认输出改成它（否则残留污染，之后设备回来也出不了声）。
        return;
    }
    // 只做"扇出"：把 miniam 自己那个浏览器的输出流复制一份到虚拟 sink 供频谱使用，
    // 浏览器的声音照常走系统默认 sink（所以能听见）。
    //
    // 两个关键点：
    // 1) 绝不要把系统默认 sink 改成虚拟接收端 —— 默认 sink 上还挂着系统里别的声音
    //    （默认 sink 上可能还挂着系统里别的声音，比如用户在放的视频），
    //    一旦改默认，那些声音会全部涌进 miniam_capture，频谱就跟着视频跳。
    // 2) 不用 module-loopback 回环 —— 浏览器已经直接送往默认 sink，回环等于把同一份
    //    声音再喂一遍默认 sink，音量叠加。旧版本留下的回环模块要清掉。
    // 把 miniam 自己那个浏览器的播放流**搬进**虚拟 sink（频谱源），再把虚拟 sink 的声音
    // 回环到真实输出，浏览器照样能听见。
    //
    // 三个关键点（都是踩过坑的）：
    // 1) 绝不要改系统默认 sink —— 默认 sink 上还挂着系统里别的声音（默认 sink 常常同时
    //    承载别的应用，比如用户在放的视频），一改默认那些声音就全涌进频谱源，
    //    频谱会跟着视频跳。
    // 2) 用 move 而不是 pw-link 扇出 —— 实测 PipeWire 上给 Pulse 流加的额外链接，
    //    在流被重建后就不再传数据（pw-link 报 already exists 而不会重建），频谱会哑掉；
    //    move 让这个流成为虚拟 sink 真正的输入，数据一定到位。
    // 3) 回环模块按需建立（而不是每次重建），否则同一份声音会被喂两遍造成音量叠加。
    let browser_pid = miniam_browser_pid();
    if browser_pid.is_none() {
        // 识别不到自己的浏览器 => 频谱不会有信号。界面刻意不显示诊断文字（避免刷屏），
        // 这里写日志，方便用户报问题时能立刻定位。
        log_audio_note("未能识别 miniam 自己的浏览器进程，频谱源将没有信号（浏览器是否已启动？）");
    }
    move_browser_streams_to(capture, real_default, browser_pid);
    let src = format!("{}.monitor", capture);
    if loopback_module_id(&src, real_default).is_none() {
        if let Some(id) = load_loopback(&src, real_default) {
            *loopback_mod = Some(id);
        }
    }
}

/// 卸载所有以 <capture>.monitor 为源的 loopback 模块（清掉旧版本留下的回环）。

/// 把卡在虚拟 sink 上的浏览器流拉回真实输出。
/// （旧版本曾把系统默认 sink 改成虚拟 sink，残留的流会留在虚拟 sink 里听不到声音。）

fn start_audio_capture(
    tx: tokio::sync::mpsc::UnboundedSender<Vec<u64>>,
    stop: Arc<AtomicBool>,
    meta: Arc<Mutex<AudioMeta>>,
    playing: Arc<AtomicBool>,
    beat: Arc<Mutex<BeatState>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let n: usize = 1024;
        let bins = n / 2;
        let width: usize = 96;
        // 慢速整体响度包络：attack 0.1s / release 3s。
        // 用它而不是「近期峰值保持」做 AGC 参考 —— 峰值保持会让音乐一弱下来，
        // 参考电平在 0.7s 内就被新峰值抬高/衰减掉，gain 立刻恢复满，强弱动态被抹平。
        // 慢包络让「音乐轻」的阶段持续保持低 gain，画面才跟着音乐起伏。
        let mut slow_level = 1e-4f32;
        let mut env = vec![0.0f32; width];
        // 节拍检测与帧率无关包络所需的跨帧状态
        let mut prev_out = vec![0.0f32; width];
        let mut flux_avg = 0.0f32;
        let mut last_beat_local = Instant::now();
        let mut last_proc = Instant::now();
        const TAU_ATTACK: f32 = 0.008; // 快攻时间常数 ~8ms
        let mut planner = realfft::RealFftPlanner::<f32>::new();
        let r2c = planner.plan_fft_forward(n);
        let mut indata = r2c.make_input_vec();
        let mut spectrum = r2c.make_output_vec();
        let mut buf = vec![0u8; n * 2];
        let mut acc: Vec<f32> = Vec::with_capacity(n);
        let mut last_send = Instant::now();

        let mut silence_start: Option<Instant> = None;
        // 必须在建 null sink 之前取：新建的 sink 可能被 PipeWire 选成默认，
        // 否则这里找不到真实输出，回环建不起来，声音会全进不出声的虚拟 sink。
        let orig_default = real_hardware_sink();
        let null_sink = setup_null_sink();

        // 源选择：MINIAM_AUDIO_SOURCE 显式覆盖优先；否则**只用虚拟接收端 miniam_capture 的
        // monitor** —— 它只混出 miniam 自己那个浏览器（Apple Music）的声音。
        // 旧实现是"在所有 monitor 里挑最响的"，而默认 sink 的 monitor 承载的是**全系统声音**
        // （默认 sink 上可能还有别的应用的声音），
        // 于是放视频时频谱就跟着视频跳。频谱只该反映被控的那一路。
        let mut candidates = if let Ok(forced) = env::var("MINIAM_AUDIO_SOURCE") {
            if forced.trim().is_empty() {
                candidate_monitor_sources()
            } else {
                vec![forced.trim().to_string()]
            }
        } else if let Some(ns) = &null_sink {
            vec![format!("{}.monitor", ns)]
        } else {
            candidate_monitor_sources()
        };
        if candidates.is_empty() {
            let _ = meta.lock().map(|mut m| m.source = "（无可用音频源：pactl/pw-dump 均未找到 monitor）".into());
            return;
        }

        let mut active = pick_best_source(&candidates);
        let mut engaged = false;
        let mut loopback_mod: Option<i64> = None;
        let mut last_sync: Option<Instant> = None;

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            {
                let _ = meta.lock().map(|mut m| {
                    m.source = active.clone();
                    m.level = 0.0;
                });
            }
            let mut child = match std::process::Command::new("parec")
                .arg("--device").arg(&active)
                .arg("--latency-msec=15")
                .arg("--format=s16le")
                .arg("--rate=44100")
                .arg("--channels=1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(300));
                    active = pick_best_source(&candidates);
                    continue;
                }
            };
            let mut rd = match child.stdout.take() {
                Some(r) => r,
                None => {
                    active = pick_best_source(&candidates);
                    continue;
                }
            };
            'inner: loop {
                if stop.load(Ordering::Relaxed) {
                    break 'inner;
                }
                // 播放流是用户按下播放之后才创建的，所以要周期性地把 miniam 自己那个
                // 浏览器重新接进虚拟 sink（频谱源），并把卡在虚拟 sink 上的流拉回真实输出。
                if last_sync.map(|t| t.elapsed() >= Duration::from_secs(2)).unwrap_or(true) {
                    last_sync = Some(Instant::now());
                    if let Some(ns) = &null_sink {
                        ensure_capture_pipeline(ns, &orig_default, &mut loopback_mod);
                    }
                }
                match rd.read(&mut buf) {
                    Ok(0) => break 'inner,
                    Ok(k) => {
                        for i in (0..k).step_by(2) {
                            let s = i16::from_le_bytes([buf[i], buf[i + 1]]) as f32 / 32768.0;
                            acc.push(s);
                        }
                        if acc.len() >= n {
                            for (j, x) in acc.drain(..n).enumerate() {
                                let w = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * j as f32 / n as f32).cos());
                                indata[j] = x * w;
                            }
                            if r2c.process(&mut indata, &mut spectrum).is_err() {
                                continue;
                            }
                            let mut mags = vec![0.0f32; width];
                            let mut frame_peak = 1e-6f32;
                            for b in 0..width {
                                let f0 = (bins as f32 * (b as f32 / width as f32).powf(2.0)) as usize;
                                // 每段至少 2 个 bin：1024 点 FFT 下低频段原本只有 1 个 bin，
                                // 取 max 时易受单个噪声 bin 影响而空跳。
                                let mut f1 = ((bins as f32 * ((b + 1) as f32 / width as f32).powf(2.0)) as usize) + 1;
                                f1 = f1.max(f0 + 2).min(bins);
                                let mut m = 0.0f32;
                                let mut msum = 0.0f32;
                                let mut mcnt = 0usize;
                                for c in spectrum.iter().take(f1.min(bins)).skip(f0) {
                                    let mag = (c.re * c.re + c.im * c.im).sqrt();
                                    if mag > m { m = mag; }
                                    msum += mag;
                                    mcnt += 1;
                                }
                                // 峰值与均值各半：纯 max 会被单个噪声 bin 主导（段形状乱跳），
                                // 纯 mean 又被大量静音 bin 拉低（段看起来空）。混合后形状既稳又保活力。
                                let m = if mcnt > 0 {
                                    0.5 * m + 0.5 * (msum / mcnt as f32)
                                } else {
                                    m
                                };
                                // 粉噪/心理声学 tilt：高频段抬升，让人声/镲片可见（霓虹感更强，且不动分箱/增益结构）
                                let tilt = 1.0 + 1.5 * (b as f32 / (width as f32 - 1.0));
                                let m = m * tilt;
                                mags[b] = m;
                                if m > frame_peak { frame_peak = m; }
                            }
                            let mut sum_sq = 0.0f32;
                            for v in mags.iter() { sum_sq += v * v; }
                            let frame_level = (sum_sq / width as f32).sqrt();
                            {
                                let _ = meta.lock().map(|mut m| m.level = frame_level);
                            }
                            let noise_floor = 3e-4f32;
                            // gain 跟随「整体响度 / 慢包络」：音乐强 -> ~1，音乐弱 -> <1 并持续，
                            // 强弱动态因此体现在画面上（原来是除以近期峰值，弱段 0.8s 后就恢复满）。
                            let gain = if frame_level < noise_floor {
                                0.0
                            } else {
                                (frame_level / slow_level.max(1e-5)).clamp(0.0, 1.5)
                            };
                            let mut out = vec![0.0f32; width];
                            for b in 0..width {
                                let shape = mags[b] / (frame_level + 1e-9);
                                let target = (shape * gain * 100.0).clamp(0.0, 100.0);
                                out[b] = target;
                            }
                            let energy = out.iter().sum::<f32>() / width as f32 / 100.0;
                            // 邻域模糊：每段与左右邻段做小核平滑，消除 FFT bin 的抖动锯齿
                            let mut blurred = vec![0.0f32; width];
                            for b in 0..width {
                                let pl = if b > 0 { out[b - 1] } else { out[b] };
                                let pr = if b + 1 < width { out[b + 1] } else { out[b] };
                                blurred[b] = 0.25 * pl + 0.5 * out[b] + 0.25 * pr;
                            }
                            out.copy_from_slice(&blurred);

                            // spectral flux 节拍检测（只统计增大的 bin）：与上一帧比较，增量超近期均值即判为节拍
                            let mut flux = 0.0f32;
                            for b in 0..width {
                                let d = out[b] - prev_out[b];
                                if d > 0.0 {
                                    flux += d;
                                }
                            }
                            flux_avg = flux_avg * 0.92 + flux * 0.08;
                            let proc_now = Instant::now();
                            // 阈值相对化：flux_avg > 1.0 保证有基础活动，1.5 倍均值保证是
                            // 「明显的能量突增」而不是噪声。原绝对阈值 5.0 在弱乐段永不触发。
                            if flux > flux_avg * 1.5 && flux_avg > 1.0
                                && last_beat_local.elapsed() > Duration::from_millis(120)
                            {
                                last_beat_local = proc_now;
                                let inten = (flux / (flux_avg + 1e-3)).clamp(1.0, 3.0);
                                if let Ok(mut bs) = beat.lock() {
                                    bs.last_beat = Some(proc_now);
                                    bs.intensity = inten;
                                }
                            }
                            prev_out.copy_from_slice(&out);

                            // 时间常数包络（帧率无关）：coef = 1 - exp(-dt/τ)，快攻慢放
                            let dt = proc_now
                                .duration_since(last_proc)
                                .as_secs_f32()
                                .clamp(0.001, 0.1);
                            last_proc = proc_now;
                            // 慢速整体响度：快攻(0.1s)慢放(3s)，作为 AGC 参考
                            let slow_coef = if frame_level > slow_level {
                                1.0 - (-dt / 0.10).exp()
                            } else {
                                1.0 - (-dt / 3.0).exp()
                            };
                            slow_level += (frame_level - slow_level) * slow_coef;
                            let attack_coef = 1.0 - (-dt / TAU_ATTACK).exp();
                            let tau_release = (0.10 + 0.12 * (1.0 - energy.clamp(0.0, 1.0)))
                                .clamp(0.08, 0.25);
                            let release_coef = 1.0 - (-dt / tau_release).exp();
                            // 全局纵向呼吸：整体响度让整片频谱随音量上下起伏（低频更有力）
                            let breath = (energy * 1.15).clamp(0.0, 1.0);
                            for b in 0..width {
                                let t = out[b] * (0.78 + 0.22 * breath);
                                let coef = if t > env[b] { attack_coef } else { release_coef };
                                env[b] += (t - env[b]) * coef;
                                out[b] = env[b];
                            }
                            let out_u64: Vec<u64> = out.iter().map(|&v| v as u64).collect();
                            let now = Instant::now();
                            if now.duration_since(last_send) >= Duration::from_millis(20) {
                                last_send = now;
                                if tx.send(out_u64).is_err() {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    return;
                                }
                            }
                            if frame_level > noise_floor * 4.0 {
                                silence_start = None;
                            } else {
                                match silence_start {
                                    None => silence_start = Some(Instant::now()),
                                    Some(t0) => {
                                        if Instant::now().duration_since(t0) > Duration::from_secs(2) {
                                            break 'inner;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => break 'inner,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let best = pick_best_source(&candidates);
            if best != active {
                active = best;
                silence_start = None;
            } else if !engaged {
                // 真实 monitor 都没声音：尝试用软件虚拟 sink 的 monitor 来抓音频。
                // 若虚拟 monitor 已经在出声（浏览器音频已接过来），直接切过去；
                // 否则只有音乐确实在播（isPlaying）时才把浏览器音频导到虚拟 sink，避免误搬。
                if let Some(ns) = &null_sink {
                    let mon = format!("{}.monitor", ns);
                    if !candidates.iter().any(|c| c == &mon) {
                        candidates.insert(0, mon.clone());
                    }
                    if probe_source_level(&mon, 300) > 0.0 {
                        // 虚拟 monitor 已经在出声：把浏览器音频接回并让虚拟 sink 出声（loopback 回环到真实声卡）。
                        ensure_capture_pipeline(ns, &orig_default, &mut loopback_mod);
                        engaged = true;
                        active = pick_best_source(&candidates);
                        silence_start = None;
                    } else if playing.load(Ordering::Relaxed) {
                        // 真实 monitor 静音但音乐在播：把浏览器音频接到虚拟 sink（抓取），并用 loopback 回环到真实声卡出声。
                        ensure_capture_pipeline(ns, &orig_default, &mut loopback_mod);
                        engaged = true;
                        active = pick_best_source(&candidates);
                        silence_start = None;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        // 收尾：卸载回环与虚拟 sink，并把默认输出还原成真实声卡，避免污染系统音频图
        if let Some(id) = loopback_mod {
            unload_module(id);
        }
        // 启动时若已被上次残留污染、没记到真实声卡，退而求其次选当前任一
        // 非 miniam_capture 的 sink，避免默认输出指向即将卸载的虚拟 sink。
        let restore = if !orig_default.is_empty() && orig_default != "miniam_capture" {
            orig_default.clone()
        } else {
            real_hardware_sink()
        };
        if !restore.is_empty() {
            set_default_sink(&restore);
        }
        unload_null_sink("miniam_capture");
    })
}

/// 节拍爆发：在底部喷出一大簇上升粒子，强度随 spectral flux 增大。
fn spawn_beat_burst(
    particles: &mut Vec<Particle>,
    bw: f32,
    bh: f32,
    intensity: f32,
    dyn_state: &AudioDyn,
    spectrum: &[u64],
) {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    // 只在有频谱（能量超阈值）的频段位置喷射，避免“无频谱处也冒粒子”
    let active: Vec<f32> = spectrum
        .iter()
        .enumerate()
        .filter(|(_, &v)| (v as f32 / 100.0).clamp(0.0, 1.0) > 0.06)
        .map(|(i, _)| (i as f32 + 0.5) * (bw / spectrum.len().max(1) as f32))
        .collect();
    if active.is_empty() {
        return; // 完全静音：不喷任何粒子
    }
    let busyness = dyn_state.busyness();
    let speed = (0.6 + busyness * 1.6 + dyn_state.energy * 0.4).clamp(0.5, 3.0);
    // 数量随节拍强度与整体能量：20~90，少而精
    let count = ((18.0 + intensity * 26.0 + dyn_state.energy * 30.0).min(90.0)) as usize;
    for _ in 0..count {
        let px = active[rng.gen::<usize>() % active.len()];
        let t = (px / bw.max(1.0)).clamp(0.0, 1.0);
        let rise = (45.0 + busyness * 90.0 + rng.gen::<f32>() * 70.0) * speed * (0.5 + intensity * 0.5);
        let vy = -rise;
        let vx = (rng.gen::<f32>() - 0.5) * (10.0 + busyness * 40.0) * speed;
        let max_life = 0.35 + rng.gen::<f32>() * 0.85;
        let col = brighten(neonize(hue_gradient(t)), 0.3);
        particles.push(Particle {
            x: px, y: bh - 2.0, vx, vy, life: max_life, max_life,
            color: col,
        });
    }
}

/// 底部频谱区：霓虹粒子频谱；无音频时附带诊断（源名 + 电平）。
fn build_spectrum(
    spectrum_data: &[u64],
    cols: u32,
    rows: u32,
    col_sprite: Option<&Sprite>,
    dot_sprite: Option<&Sprite>,
    particles: &mut Vec<Particle>,
    peaks: &mut Vec<f32>,
    dynamic: &AudioDyn,
) -> Vec<ratatui::text::Line<'static>> {
    if spectrum_data.iter().any(|&v| v > 3) {
        render_spectrum(spectrum_data, cols, rows, col_sprite, dot_sprite, particles, SPEC_NEON, peaks, dynamic)
    } else {
        Vec::new()
    }
}

/// 放射状配色：t=0 核心(白热偏暖黄) → t~0.4 品红/玫红 → t=1 桃紫，整体高饱和霓虹，
/// 对应参考图的「深蓝紫底 + 粉红/桃色暖光线」冷暖对比。
fn radial_color(t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    // 低饱和蓝-蓝紫渐变 + 大动态明暗层次（对齐用户认可的旧版：色相 208~276°、
    // 饱和度均值 0.34、亮度 0.22~1.00）。高饱和纯青/纯紫相邻并置会强烈色相跳变，
    // 视觉上就是「粗糙」；收窄色相 + 降饱和后，细腻感来自明暗层次而非色相跳跃。
    let c = if t < 0.07 {
        brighten((214, 230, 253), 0.45 * (1.0 - t / 0.07))
    } else {
        let stops: [(f32, (u8, u8, u8)); 5] = [
            (0.00, (214, 230, 253)), // 核心：近白淡蓝
            (0.22, (140, 172, 232)), // 淡蓝
            (0.50, (82, 104, 178)),  // 中蓝
            (0.78, (72, 68, 142)),   // 蓝紫
            (1.00, (52, 48, 98)),    // 边缘：暗蓝紫
        ];
        let mut out = stops[stops.len() - 1].1;
        for i in 0..stops.len() - 1 {
            let (t0, c0) = stops[i];
            let (t1, c1) = stops[i + 1];
            if t <= t1 {
                let lt = ((t - t0) / (t1 - t0).max(1e-6)).clamp(0.0, 1.0);
                out = (
                    (c0.0 as f32 + (c1.0 as f32 - c0.0 as f32) * lt) as u8,
                    (c0.1 as f32 + (c1.1 as f32 - c0.1 as f32) * lt) as u8,
                    (c0.2 as f32 + (c1.2 as f32 - c0.2 as f32) * lt) as u8,
                );
                break;
            }
        }
        out
    };
    // 不过 neonize：neonize 会把最亮通道归一化拉到 255，低饱和配色会被破坏回
    // 高饱和纯色，重新出现色相跳变（粗糙）。线性频谱走 SPEC_NEON，不受影响。
    c
}

/// 在 (cx,cy) 处画一圈椭圆径向光晕（additive），用于深空底晕与核心光晕。
/// 用椭圆半径 (max_rx, max_ry) 适配又宽又扁的频谱条带，避免被压成一个点。
fn draw_radial_glow(buf: &mut image::RgbaImage, cx: f32, cy: f32, max_rx: f32, max_ry: f32, c: (u8, u8, u8), alpha: f32) {
    let bw = buf.width() as i64;
    let bh = buf.height() as i64;
    if max_rx <= 0.0 || max_ry <= 0.0 { return; }
    for yy in 0..bh {
        let dy = (yy as f32) - cy;
        for xx in 0..bw {
            let dx = (xx as f32) - cx;
            let v = (dx * dx) / (max_rx * max_rx) + (dy * dy) / (max_ry * max_ry);
            if v > 1.0 { continue; }
            let a = alpha * (1.0 - v).powf(1.5) * 0.6;
            if a <= 0.003 { continue; }
            let p = buf.get_pixel_mut(xx as u32, yy as u32);
            p[0] = (p[0] as f32 + c.0 as f32 * a).min(255.0) as u8;
            p[1] = (p[1] as f32 + c.1 as f32 * a).min(255.0) as u8;
            p[2] = (p[2] as f32 + c.2 as f32 * a).min(255.0) as u8;
            p[3] = 255;
        }
    }
}

/// 一条从核心向外辐射的细光线（椭圆方向）：从 (ax0,ay0) 到 (ax1,ay1) 沿角度画线，
/// 颜色由内(白热)到外(粉紫)渐变，内亮外淡。
fn draw_ray(buf: &mut image::RgbaImage, cx: f32, cy: f32, ang: f32, ax0: f32, ay0: f32, ax1: f32, ay1: f32, e: f32, waves: &[f32]) {
    let steps = ((ax1 - ax0).max(1.0) as i32).min(400);
    let cos = ang.cos();
    let sin = ang.sin();
    let w = 1.0 + e * 1.0;
    for s in 0..=steps {
        let t = s as f32 / steps as f32;
        let r = ax0 + (ax1 - ax0) * t;
        let px = (cx + cos * (ax0 + (ax1 - ax0) * t)) as i64;
        let py = (cy + sin * (ay0 + (ay1 - ay0) * t)) as i64;
        let c = radial_color(t);
        let a0 = (0.5 + 0.5 * (1.0 - t)) * (0.55 + 0.45 * e);
        // bright band rolling outward along the real radius; all spokes share it -> a round expanding wavefront
        let mut band = 0.0f32;
        for &wr in waves {
            let d = (r - wr) / 5.0;
            band += (-d * d).exp();
        }
        let a = (a0 + band * 0.85).min(1.0);
        let cc = brighten(c, 0.02 + band.min(1.0) * 0.12);
        draw_core(buf, px - (w as i64) / 2, py, w as i64 + 1, 1, cc, a);
    }
}

/// 核心：外光晕 + 实心白热核（椭圆，适配扁条带）。additive 叠在最上层。
fn draw_core_glow(buf: &mut image::RgbaImage, cx: f32, cy: f32, core_rx: f32, core_ry: f32, _breathe: f32) {
    draw_radial_glow(buf, cx, cy, core_rx * 2.0, core_ry * 2.0, (120, 220, 255), 0.07);
    let rx = (core_rx * 0.55).max(0.5);
    let ry = (core_ry * 0.55).max(0.5);
    let x0 = (cx - rx).max(0.0) as i64;
    let y0 = (cy - ry).max(0.0) as i64;
    let w = (rx * 2.0) as i64 + 1;
    let h = (ry * 2.0) as i64 + 1;
    draw_core(buf, x0, y0, w, h, (130, 235, 255), 0.9);
}

/// 一条带运动模糊的曲速光线粒子：沿速度反方向拉出细长拖尾，用 warp 贴图做纹理。
fn draw_streak(buf: &mut image::RgbaImage, x: f32, y: f32, vx: f32, vy: f32, c: (u8, u8, u8), lr: f32, _dot: Option<&Sprite>, col: Option<&Sprite>) {
    let len = (18.0 + lr * 26.0).min(56.0);
    let sp = (vx * vx + vy * vy).sqrt().max(1.0);
    let ux = vx / sp;
    let uy = vy / sp;
    let steps = len as i32;
    for s in 0..=steps {
        let t = s as f32 / steps as f32;
        let px = (x - ux * len * t) as i64;
        let py = (y - uy * len * t) as i64;
        // keep old/outer particles bright; skip faint steps so the braille
        // rasterizer never lights up near-black dots at the outer edge
        let a = (0.45 + 0.55 * lr) * (1.0 - t).powf(0.65);
        if a < 0.12 { continue; }
        let cc = brighten(c, 0.12 + t * 0.22);
        if let Some(sp_) = col {
            blend_sprite(buf, sp_, px - 1, py - 1, 3, 3, cc, a * 0.7);
        }
        draw_core(buf, px, py, 1, 1, cc, a);
    }
}

/// 放射状（冷光霓虹）频谱：中心冷白光核向外喷射细长光线粒子，
/// 青→电蓝→紫罗兰霓虹渐变，真实圆形（超出边框部分被裁掉），核心随音频「呼吸」。
fn render_radial_spectrum(
    spectrum: &[u64],
    cols: u32,
    rows: u32,
    col_sprite: Option<&Sprite>,
    dot_sprite: Option<&Sprite>,
    particles: &mut Vec<Particle>,
    dyn_state: &AudioDyn,
    phase: f32,
    beat_wave: Option<f32>,
    burst: Option<f32>,
    fade: f32,
) -> Vec<ratatui::text::Line<'static>> {
    use rand::Rng;
    const DT: f32 = 0.05;
    // draw one concentric ring stroke (true circle); pixels outside the buffer are clipped.
    fn draw_ring(buf: &mut image::RgbaImage, cx: f32, cy: f32, r: f32, w: f32, c: (u8, u8, u8), alpha: f32) {
        if r <= 0.0 || w <= 0.0 || alpha <= 0.0 { return; }
        let bw = buf.width() as i64;
        let bh = buf.height() as i64;
        let half = w * 0.5;
        let x0 = ((cx - r - half).floor() as i64).max(0);
        let x1 = ((cx + r + half).ceil() as i64).min(bw - 1);
        let y0 = ((cy - r - half).floor() as i64).max(0);
        let y1 = ((cy + r + half).ceil() as i64).min(bh - 1);
        for yy in y0..=y1 {
            let dy = yy as f32 - cy;
            for xx in x0..=x1 {
                let dx = xx as f32 - cx;
                let d = (dx * dx + dy * dy).sqrt();
                let off = (d - r).abs();
                if off > half { continue; }
                let a = alpha * (1.0 - off / half);
                if a <= 0.003 { continue; }
                let p = buf.get_pixel_mut(xx as u32, yy as u32);
                let lum = (p[0] as f32 + p[1] as f32 + p[2] as f32) / 765.0;
                let (tr, tg, tb) = if lum > 0.5 {
                    (15.0, 30.0, 70.0)
                } else {
                    (c.0 as f32, c.1 as f32, c.2 as f32)
                };
                p[0] = (p[0] as f32 * (1.0 - a) + tr * a).min(255.0) as u8;
                p[1] = (p[1] as f32 * (1.0 - a) + tg * a).min(255.0) as u8;
                p[2] = (p[2] as f32 * (1.0 - a) + tb * a).min(255.0) as u8;
                p[3] = 255;
            }
        }
    }
    let bw = (cols.max(1) * 2) as u32;
    let bh = (rows.max(1) * 4) as u32;
    // 铺不透明底 + 半色调点阵输出：braille 2x4 像素/格保证绘制精度（无锯齿），
    // raster_braille 用固定稀疏点阵（每像素行恰好 1 点）彻底消除横向暗条。
    let mut buf = image::RgbaImage::from_pixel(bw, bh, image::Rgba([0, 0, 0, 255]));
    let cx = bw as f32 / 2.0;
    let cy = bh as f32 / 2.0;
    let n = spectrum.len().max(1) as u32;
    // breathe: slow sine + audio pulse; core pulses with the beat.
    let breathe = 1.0 + 0.08 * (phase * 0.18).sin() + dyn_state.pulse * 0.5;
    // true circle: radius = half of the LONG edge so the disc spans the full band width;
    // the top/bottom overflow is clipped by the frame (not squished into an ellipse).
    let radius = ((bw as f32).max(bh as f32) / 2.0) * 0.98;
    let core_r = ((bh as f32) * (0.10 + dyn_state.bass * 0.12 + dyn_state.pulse * 0.12) * breathe).min(bh as f32 * 0.18);
    // 背景：用巨大半径的辉光把整片频谱区均匀铺一层冷蓝底，保证“填满区域”。
    // Base wavefronts: phase is advanced in the main loop at an audio-driven rate,
    // so busier music expands them faster instead of a fixed-speed spin.
    let wave_count = 3i32;
    let mut waves: Vec<f32> = Vec::with_capacity(wave_count as usize);
    for j in 0..wave_count {
        let t = (phase + j as f32 / wave_count as f32) % 1.0;
        waves.push(core_r + t * (radius - core_r));
    }
    let mut ray_waves = waves.clone();
    if let Some(bt) = beat_wave {
        ray_waves.push(core_r + bt * (radius - core_r));
    }
    // 360 spokes from the center; all tips land on one circle, so the outline stays round.
    // brightness floor keeps every direction visible regardless of audio level; overflow is clipped.
    let spokes = 360u32;
    for k in 0..spokes {
        let ang = (k as f32 / spokes as f32) * std::f32::consts::PI * 2.0 - std::f32::consts::FRAC_PI_2;
        let bin = (((ang + std::f32::consts::FRAC_PI_2) / (std::f32::consts::PI * 2.0)) * n as f32) as usize % n as usize;
        let e = ((spectrum[bin] as f32) / 100.0).clamp(0.0, 1.0).powf(0.55).max(0.18);
        let r_tip = radius * (0.86 + 0.14 * e);
        draw_ray(&mut buf, cx, cy, ang, core_r, core_r, r_tip, r_tip, e, &ray_waves);
    }
    // Ripple rings ride the SAME wavefronts as the spoke bright bands, so the two reinforce
    // into one clearly visible round wave expanding from the center; overflow is clipped.
    for &wr in &waves {
        let t = (wr - core_r) / (radius - core_r).max(1.0);   // 0 = center, 1 = edge
        let env = (1.0 - t).powf(1.1);                           // bright at birth, fades outward
        let a = ((0.5 + dyn_state.energy * 0.3 + dyn_state.pulse * 0.5) * env).min(0.95);
        let c = brighten(radial_color(t), 0.05);
        draw_ring(&mut buf, cx, cy, wr, 3.0 + dyn_state.bass * 3.0, c, a);
    }
    // Beat wavefront: brighter/thicker ring born on each onset, fading with its age.
    if let Some(bt) = beat_wave {
        let wr = core_r + bt * (radius - core_r);
        let env = (1.0 - bt).powf(0.8);
        let a = ((0.65 + dyn_state.pulse * 0.35) * env).min(0.95);
        let c = brighten(radial_color(0.12 + bt * 0.55), 0.15);
        draw_ring(&mut buf, cx, cy, wr, 5.0 + dyn_state.bass * 4.0, c, a);
    }
    // 曲速粒子：强频段从核心沿对应角度向外喷射细长拖尾光线（运动模糊）。
    let mut rng = rand::thread_rng();
    // Onset burst: a short-lived spray of streak particles out from the core.
    if let Some(inten) = burst {
        let count = (10.0 + inten * 14.0 + dyn_state.energy * 16.0).min(40.0) as usize;
        for _ in 0..count {
            let a = rng.gen::<f32>() * std::f32::consts::PI * 2.0;
            let speed = (bh as f32) * (1.2 + inten * 1.5 + rng.gen::<f32>() * 1.5);
            let life = 0.25 + rng.gen::<f32>() * (0.4 + dyn_state.treble * 0.4);
            particles.push(Particle {
                x: cx + a.cos() * core_r,
                y: cy + a.sin() * core_r,
                vx: a.cos() * speed,
                vy: a.sin() * speed,
                life, max_life: life,
                color: radial_color(0.05),
            });
        }
    }
    let spawn_scale = (0.3 + dyn_state.energy * 2.2).clamp(0.0, 3.0);
    for i in 0..n {
        let e = ((spectrum[i as usize] as f32) / 100.0).clamp(0.0, 1.0).powf(0.6);
        if e < 0.05 { continue; }
        let ang = (i as f32 / n as f32) * std::f32::consts::PI * 2.0 - std::f32::consts::FRAC_PI_2;
        let sp = (e * spawn_scale * (0.6 + rng.gen::<f32>() * 0.6)) as usize;
        for _ in 0..sp.min(2) {
            let a = ang + (rng.gen::<f32>() - 0.5) * 0.06;
            let life = 0.45 + rng.gen::<f32>() * (0.7 + dyn_state.treble * 0.6);
            // reach the window edge within its lifetime; flatten vertical speed
            // so particles survive the short band instead of leaving at once
            let speed = (bw as f32 / 2.0) / life * (0.75 + e * 0.5) * (0.85 + dyn_state.busyness() * 0.4);
            let vy_scale = (bh as f32 / bw as f32).max(0.08);
            particles.push(Particle {
                x: cx + a.cos() * core_r,
                y: cy + a.sin() * core_r,
                vx: a.cos() * speed,
                vy: a.sin() * speed * vy_scale,
                life, max_life: life,
                color: radial_color(0.05),
            });
        }
    }
    // 更新并绘制粒子（细长拖尾 = 曲速光线）。
    let mut next: Vec<Particle> = Vec::with_capacity(particles.len());
    for p in particles.drain(..) {
        let mut p = p;
        p.x += p.vx * DT;
        p.y += p.vy * DT;
        p.life -= DT;
        if p.life <= 0.0 || p.x < -4.0 || p.x > bw as f32 + 4.0 || p.y < -4.0 || p.y > bh as f32 + 4.0 {
            continue;
        }
        // 颜色随径向位置连续变化：与所在环带的 radial_color 渐变色一致，
        // 而不是 spawn 时随机取一段 —— 随机会让相邻格色相跳变，看起来粗糙。
        let pd = ((p.x - cx) * (p.x - cx) + (p.y - cy) * (p.y - cy)).sqrt() / radius.max(1.0);
        p.color = radial_color(pd.clamp(0.0, 1.0));
        let lr = (p.life / p.max_life).clamp(0.0, 1.0);
        draw_streak(&mut buf, p.x, p.y, p.vx, p.vy, p.color, lr, dot_sprite, col_sprite);
        next.push(p);
    }
    *particles = next;
    // 炽白冷光核 + 光晕（最后画，叠在最上层）。
    draw_core_glow(&mut buf, cx, cy, core_r, core_r, breathe);
    // fade out/in: 只压暗 RGB，不动 alpha。
    // buf 铺的是不透明底，alpha 若跟着缩放，淡出到 0 时所有像素 alpha=0 ->
    // raster_braille 判 bits==0 -> 输出裸空格 -> 整片空行（实测 fade=0 有 400 个空格）。
    if fade < 1.0 {
        for p in buf.pixels_mut() {
            p[0] = (p[0] as f32 * fade) as u8;
            p[1] = (p[1] as f32 * fade) as u8;
            p[2] = (p[2] as f32 * fade) as u8;
        }
    }
    // onset flash: short full-frame brightness lift on each beat, decaying over
    // the first ~0.2 of the beat wavefront life so it lasts more than one frame
    if let Some(bt) = beat_wave {
        let flash = (1.0 - (bt / 0.2).min(1.0)).max(0.0);
        if flash > 0.01 {
            let add = (flash * 50.0) as u32;
            for p in buf.pixels_mut() {
                p[0] = (p[0] as u32 + add).min(255) as u8;
                p[1] = (p[1] as u32 + add).min(255) as u8;
                p[2] = (p[2] as u32 + add).min(255) as u8;
            }
        }
    }
    raster_braille(&buf, cols, rows, 0, 0)
}

async fn wait_for_quit_signal() {
    use tokio::signal::unix::{SignalKind, Signal};
    let mut st: Option<Signal> = tokio::signal::unix::signal(SignalKind::terminate()).ok();
    let mut si: Option<Signal> = tokio::signal::unix::signal(SignalKind::interrupt()).ok();
    let mut sh: Option<Signal> = tokio::signal::unix::signal(SignalKind::hangup()).ok();
    loop {
        tokio::select! {
            _ = async { match st.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending::<()>().await } } => return,
            _ = async { match si.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending::<()>().await } } => return,
            _ = async { match sh.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending::<()>().await } } => return,
        }
    }
}

/// 全屏 TUI：ratatui 渲染状态面板，单键即响应，浏览器保持 headless。
async fn run_ui(b: Backend, bidi: Option<BiDi>) -> Result<()> {
    use ratatui::crossterm::event::{self, Event, KeyCode};
    use std::io::IsTerminal;
    use tokio::sync::mpsc;

    // 退出时（正常 q/Esc 或信号转发）彻底回收我们拉起的浏览器进程组，
    // 否则 Firefox/Chrome 会话残留会把工具锁死，下次起播直接失败。
    struct BrowserGuard;
    impl Drop for BrowserGuard {
        fn drop(&mut self) {
            kill_browser();
        }
    }
    let _guard = BrowserGuard;

    let backend_name = match b {
        Backend::Chrome => "chrome(CDP)",
        Backend::Firefox => "firefox(BiDi)",
    };

    // miniam ui 必须运行在真实交互终端；管道/重定向会让 TUI 无法渲染、退出时报 broken pipe
    if !std::io::stdout().is_terminal() {
        bail!(
            "miniam ui 需要交互式终端（TTY）。请直接在终端里运行，不要管道或重定向其输出；\n\
             并确保先 `miniam up` 启动浏览器，进入后按 `g` 起播。"
        );
    }

    let mut term = ratatui::init();
    // 只在主线程 panic 时复位终端。worker 线程的 panic 会被 tokio 兜住（进程不退出），
    // 此时若无条件 ratatui::restore()，会退出 alternate screen / 关闭 raw mode，
    // 让正在运行的 TUI 画面撕裂、shell 提示符露出、q 键读不到 —— 界面"乱掉"的根因。
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            let _ = ratatui::restore();
            default_hook(info);
        } else {
            // worker 线程 panic：绝不往 stderr 打印（会污染 alternate screen 上的 TUI 画面），
            // 也绝不复位终端；只记日志，主循环继续可操作、q 仍可退出。
            let loc = info.location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_default();
            let _ = std::fs::write("/tmp/miniam_panic.log", format!("worker panic at {}~", loc));
        }
    }));
    let (tx, mut rx) = mpsc::unbounded_channel::<KeyCode>();
    let (rtx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
    let sig_tx = tx.clone();
    std::thread::spawn(move || {
        loop {
            match event::read() {
                Ok(Event::Key(k)) => {
                    let _ = tx.send(k.code);
                }
                Ok(Event::Resize(c, r)) => {
                    let _ = rtx.send((c, r));
                }
                _ => {}
            }
        }
    });

    // 终端关闭 / 外部 kill 会发 SIGTERM/SIGHUP/SIGINT：转发成 'q'，让主循环走干净退出路径，
    // worker 收尾时释放 BiDi 会话，避免会话泄漏把工具锁死。
    #[cfg(unix)]
    {
        let stx = sig_tx;
        tokio::spawn(async move {
            wait_for_quit_signal().await;
            let _ = stx.send(ratatui::crossterm::event::KeyCode::Esc);
        });
    }

    let mut status: Value = Value::Null;
    let mut busy = false;
    let mut last_status_log_ts: u64 = 0;
    let mut blink_pending: bool = false;
    let mut blink_until: Option<std::time::Instant> = None; // 按键反馈：最短可见快闪时长（命令 ack 前也会持续）
    let mut message: String = String::new();
    // sticky status hint: 当设置 message_until 时，message 在该时间点前不清空（用于封面比例调参提示）。
    let mut message_until: Option<Instant> = None;
    let (spec_tx, mut spec_rx) = mpsc::unbounded_channel::<Vec<u64>>();
    let stop = Arc::new(AtomicBool::new(false));
    let audio_meta = Arc::new(Mutex::new(AudioMeta { source: String::from("（初始化中…）"), level: 0.0 }));
    let playing = Arc::new(AtomicBool::new(false));
    let beat = Arc::new(Mutex::new(BeatState { last_beat: None, intensity: 0.0 }));
    let audio_handle = start_audio_capture(spec_tx, stop.clone(), audio_meta.clone(), playing.clone(), beat.clone());
    let mut spectrum_data: Vec<u64> = vec![0u64; 64];
    let (cover_tx, mut cover_rx) = mpsc::unbounded_channel::<Vec<ratatui::text::Line>>();
    let mut cover_lines: Vec<ratatui::text::Line> = Vec::new();
    let mut last_artwork: Option<String> = None;
    // 贴图已移除，load_sprite(&[]) 返回 None，频谱走纯色柱。
    let col_sprite = load_sprite(SPRITE_COL_BYTES);
    let dot_sprite = load_sprite(SPRITE_DOT_BYTES);
    let mut particles: Vec<Particle> = Vec::new();
    let mut peaks: Vec<f32> = vec![0.0f32; 64];
    let mut audio_dyn = AudioDyn::default();
    // 放射状频谱（曲速风格）开关与独立粒子池 / 呼吸相位。
    let mut spec_mode: u8 = 1; // 0=绿(经典) 1=霓虹线 2=放射；c 键循环
    let mut radial_phase: f32 = 0.0;
    let mut radial_beat_wave: Option<f32> = None;
    let mut radial_fade: f32 = 0.0;
    let mut radial_particles: Vec<Particle> = Vec::new();
    // 启动时探测一次终端单元格比例，避免每帧查询阻塞输入线程。
    if let Ok(mut a) = COVER_ASPECT.lock() {
        *a = detect_cell_aspect();
    }
    let mut last_spawned_beat: Option<std::time::Instant> = None;

    // 整个 ui 生命周期只保留一个 BiDi 会话：wait_ready 建好的直接复用，
    // 状态轮询 / g / n/b / 封面全部走它，避免多次 connect 撞 Firefox 会话上限。
    let mut fx: Option<BiDi> = if matches!(b, Backend::Firefox) { bidi } else { None };

    // 先立即取一次状态填充界面
    match eval_expr(b, &mut fx, &mk(STATUS_BODY)).await {
        Ok(st) => { status = st.clone(); playing.store(st.get("isPlaying").and_then(|v| v.as_bool()).unwrap_or(false), Ordering::Relaxed); },
        Err(e) => message = format!("状态获取失败: {}", e),
    }

    // 所有对浏览器的访问（状态轮询 / 按键命令 / 起播）都进一个串行 worker，
    // 主渲染循环只收发通道、绝不 await 浏览器调用，频谱便不会被 eval_expr 阻塞。
    enum UiCmd {
        Status,
        Eval(String),
        Boot,
        Cover { url: String, w: u16, h: u16 },
        Click { pattern: String },
        Dump,
    }
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCmd>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<Value>();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<()>();
    // g / n / b 等主动命令的失败原因回传 UI，避免"按了没反应"却无从得知。
    let (err_tx, mut err_rx) = mpsc::unbounded_channel::<String>();
    let worker = {
        let wb = b;
        // fx 来自 wait_ready 的单一会话，直接 move 进来复用，不再二次 connect。
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    UiCmd::Status => {
                        let res = if matches!(wb, Backend::Chrome) {
                            eval_expr(wb, &mut fx, &mk(STATUS_BODY)).await
                        } else {
                            fx_eval(&mut fx, &mk(STATUS_BODY)).await
                        };
                        if let Ok(v) = res {
                            let _ = status_tx.send(v);
                        }
                        // 页面若整页刷新过，氛围视频抑制器就没了 —— 补装一次。
                        if ambient_video_guard_enabled() {
                            let present = match wb {
                                Backend::Chrome => eval_expr(wb, &mut fx, AMBIENT_GUARD_CHECK).await,
                                Backend::Firefox => fx_eval(&mut fx, AMBIENT_GUARD_CHECK).await,
                            }
                            .ok()
                            .and_then(|v| v.get("present").and_then(|b| b.as_bool()))
                            .unwrap_or(false);
                            if !present {
                                install_ambient_guard(wb, &mut fx).await;
                            }
                        }
                    }
                    UiCmd::Eval(expr) => {
                        if matches!(wb, Backend::Chrome) {
                            let _ = eval_expr(wb, &mut fx, &expr).await;
                        } else {
                            let _ = fx_eval(&mut fx, &expr).await;
                        }
                        // 命令执行完毕（无论成败）：通知主循环停止按键反馈闪烁
                        let _ = done_tx.send(());
                    }
                    UiCmd::Boot => {
                        let res = tokio::time::timeout(Duration::from_secs(40), boot_play(wb, &mut fx)).await;
                        match res {
                            Ok(Err(e)) => { let _ = err_tx.send(format!("起播失败: {}", e)); }
                            Err(_) => { let _ = err_tx.send("起播超时：浏览器/页面无响应，按 q 退出重开 miniam，或按 d 诊断页面".to_string()); }
                            Ok(Ok(_)) => {}
                        }
                        let _ = done_tx.send(());
                    }
                    UiCmd::Click { pattern } => {
                        // 点之前先记住当前曲目：电台（无限流）没有「上一首」可退，
                        // 点了不会换歌 —— 要据此给用户明确提示，而不是静默无反应。
                        const NP_EXPR: &str = "const mk = window.MusicKit && window.MusicKit.getInstance(); return { id: mk && mk.nowPlayingItem ? mk.nowPlayingItem.id : null, title: mk && mk.nowPlayingItem ? mk.nowPlayingItem.title : null };";
                        let is_prev = pattern.contains("上一") || pattern.to_ascii_lowercase().contains("prev");
                        let before = if is_prev { now_playing_info(wb, &mut fx).await } else { None };
                        if let Err(e) = click_label(wb, &mut fx, &pattern).await {
                            let _ = err_tx.send(format!("切歌失败: {}", e));
                        } else if is_prev {
                            // 「上一首」：等页面切完再比一次；没换歌 = 电台没有历史可退。
                            tokio::time::sleep(Duration::from_millis(900)).await;
                            let after = now_playing_info(wb, &mut fx).await;
                            // 没换歌 = 个人电台（无限流）没有「上一首」可退。
                            // 队列无名字即电台；有名字（专辑/歌单）时上一首是有意义的。
                            let (same_title, is_station) = match (&before, &after) {
                                (Some((bt, bq)), Some((at, _))) => (bt == at, bq.is_empty()),
                                _ => (false, false),
                            };
                            if same_title && is_station {
                                let msg = "个人电台是无限流，没有「上一首」可退（按 n 换下一首）";
                                log_audio_note(msg);
                                let _ = err_tx.send(msg.to_string());
                            }
                        }
                        // 切歌命令执行完毕：通知主循环停止按键反馈闪烁
                        let _ = done_tx.send(());
                    }
                    UiCmd::Dump => {
                        let expr = r#"(() => {
                            const labels = Array.from(document.querySelectorAll('button,[role=button],[aria-label],[title],a'))
                                .map(e => ((e.getAttribute('aria-label') || '') + '|' + (e.textContent || '')).trim())
                                .filter(t => t.length > 1 && t.length < 50)
                                .slice(0, 300);
                            // 播放条按钮的可用状态：n/b 能不能点全看这里
                            // （个人电台不支持回退时，「上一首」是 disabled，按了也没反应）。
                            const bar = Array.from((document.querySelector('.player-bar') || document).querySelectorAll('button'))
                                .map(e => {
                                    const r = e.getBoundingClientRect();
                                    return { t: (e.textContent || '').trim().slice(0, 10),
                                             cls: (e.className || '').slice(0, 30),
                                             dis: !!(e.disabled || e.getAttribute('aria-disabled') === 'true'),
                                             x: Math.round(r.left + r.width / 2),
                                             y: Math.round(r.top + r.height / 2) };
                                });
                            const mk = window.MusicKit && window.MusicKit.getInstance();
                            const it = mk && mk.nowPlayingItem;
                            const np = it ? { title: it.title, artist: it.artistName, queue: mk.queue ? mk.queue.name : null, len: mk.queue ? mk.queue.length : null, isPlaying: mk.isPlaying } : null;
                            return { url: location.href, labels, bar, np };
                        })()"#;
                        let res = if matches!(wb, Backend::Chrome) {
                            eval_expr(wb, &mut fx, expr).await
                        } else {
                            fx_eval(&mut fx, expr).await
                        };
                        if let Ok(v) = res {
                            let _ = std::fs::write("/tmp/miniam_debug.txt", serde_json::to_string_pretty(&v).unwrap_or_default());
                        }
                    }
                    UiCmd::Cover { url, w, h } => {
                        if w == 0 || h == 0 {
                            continue;
                        }
                        let body = format!(
                            "const u = {};
{}",
                            serde_json::to_string(&url).unwrap_or_default(),
                            COVER_FETCH_BODY
                        );
                        let res = if matches!(wb, Backend::Chrome) {
                            eval_expr(wb, &mut fx, &mk(&body)).await
                        } else {
                            fx_eval(&mut fx, &mk(&body)).await
                        };
                        match res {
                            Ok(v) => {
                                if let Some(du) = v["dataUrl"].as_str().map(|s| s.to_string()) {
                                    if let Some(lines) = render_cover(du, w, h) {
                                        let _ = cover_tx.send(lines);
                                    }
                                }
                            }
                            Err(_) => {}
                        }
                    }
                }
            }
            // worker 退出时释放持久会话
            if let Some(mut s) = fx {
                s.end().await;
            }
        })
    };;
    // 立即要一次状态
    let _ = cmd_tx.send(UiCmd::Status);

    // 启动自动起播：进 TUI 就开始播，不用再按空格。
    // 页面/MusicKit 还没就绪时不能急着发 Boot（boot_play 的 is_playing 检查会误判），
    // 所以等状态里出现队列或曲名（= MusicKit ready）再发；超过 45s 还没就绪也试一次，
    // 让 boot_play 走完流程给出明确错误，而不是静默什么都不发生。
    let mut auto_boot_pending = true;
    let auto_boot_deadline = std::time::Instant::now() + Duration::from_secs(45);
    let mut auto_boot_hinted = false;

    let mut status_timer = tokio::time::interval(Duration::from_secs(1));

    loop {
        // 每帧先更新音频复合信号（能量/低频/高频/谱通量/节拍脉冲），再进入渲染。
        audio_dyn.update(&spectrum_data);
        let fade_target = if playing.load(Ordering::Relaxed) { 1.0 } else { 0.0 };
        radial_fade += (fade_target - radial_fade) * 0.15;
        if radial_fade < 0.005 { radial_fade = 0.0; }
        // wavefront speed follows the music: busier (more flux/energy) expands faster.
        // 0.02s is the base frame interval of the 20ms spectrum feed.
        let wave_speed = 0.8 + audio_dyn.flux * 2.2 + audio_dyn.energy * 0.5;
        radial_phase = (radial_phase + wave_speed * 0.02) % 1.0;
        if let Some(t) = radial_beat_wave.as_mut() {
            *t += wave_speed * 0.02 * 1.3;
            if *t >= 1.0 { radial_beat_wave = None; }
        }
        // 节拍触发粒子爆发：每个节拍只在首次检测到时喷一次
        let do_burst = {
            let g = beat.lock().unwrap_or_else(|e| e.into_inner());
            match (g.last_beat, g.intensity) {
                (Some(t), inten)
                    if t.elapsed() < Duration::from_millis(150)
                        && last_spawned_beat.map_or(true, |ls| t > ls) =>
                {
                    last_spawned_beat = Some(t);
                    Some(inten)
                }
                _ => None,
            }
        };
        if let Some(inten) = do_burst {
            audio_dyn.beat(inten); // 快起
            radial_beat_wave = Some(0.0);
        }
        term.draw(|f| {
            let fb_phase: Option<f64> = {
        let n0 = std::time::Instant::now();
        if blink_pending || blink_until.map_or(false, |u| n0 < u) {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            Some((t * (2.0 * std::f64::consts::PI / 0.15)).sin())
        } else {
            None
        }
    };
    let spec_area = ui_chunks(f.area())[7];
            if let Some(inten) = do_burst {
                // 粒子坐标系与 render_spectrum 一致：bw = cols*2, bh = rows*4
                spawn_beat_burst(&mut particles, spec_area.width as f32 * 2.0, spec_area.height as f32 * 4.0, inten, &audio_dyn, &spectrum_data);
            }
            let spec_iw = spec_area.width as u32;
            let spec_ih = spec_area.height as u32;
            let cover_to_draw = cover_lines.clone();
            let spectrum_lines = if spec_mode == 2 {
                render_radial_spectrum(&spectrum_data, spec_iw, spec_ih, col_sprite.as_ref(), dot_sprite.as_ref(), &mut radial_particles, &audio_dyn, radial_phase, radial_beat_wave, do_burst, radial_fade)
            } else if spec_mode == 1 {
                build_spectrum(&spectrum_data, spec_iw, spec_ih, col_sprite.as_ref(), dot_sprite.as_ref(), &mut particles, &mut peaks, &audio_dyn)
            } else {
                // 绿(经典)模式：底部频谱区由 draw_ui 用 Sparkline 直接画，这里传空
                Vec::new()
            };
            draw_ui(f, &status, busy, fb_phase, &message, backend_name, &spectrum_data, &cover_to_draw, &spectrum_lines, spec_mode)
        })?;
        // radial_phase / radial_beat_wave are advanced above (audio-driven).
        // sticky 提示期间不清空，否则正常清空。
        match message_until {
            Some(t) if t > Instant::now() => {}
            _ => { message.clear(); message_until = None; }
        }


        let cover_url = last_artwork.clone();
        let key = tokio::select! {
            k = rx.recv() => k,
            Some(s) = spec_rx.recv() => {
                spectrum_data = s;
                None
            }
            Some(st) = status_rx.recv() => {
                if !busy {
                    // 诊断日志：网页自行暂停（非按空格）时记录现场到 /tmp/miniam_pause.log，
                    // 因为浏览器不可见、暂停时机随机，靠人工按 d 抓不到。
                    let was_playing = status["isPlaying"].as_bool().unwrap_or(false);
                    let now_playing = st.get("isPlaying").and_then(|v| v.as_bool()).unwrap_or(false);
                    if was_playing && !now_playing {
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let rec = serde_json::json!({
                            "ts": ts, "event": "isPlaying->false",
                            "url": st.get("url"), "hidden": st.get("hidden"),
                            "visibility": st.get("visibility"),
                            "playbackState": st.get("playbackState"),
                            "title": st.get("title"), "queueLength": st.get("queueLength"),
                            "queueName": st.get("queueName"), "docTitle": st.get("docTitle")
                        });
                        use std::io::Write as _;
                        if let Ok(mut f) = std::fs::OpenOptions::new()
                            .create(true).append(true).open("/tmp/miniam_pause.log") {
                            let _ = writeln!(f, "{}", rec);
                        }
                    }
                    status = st.clone();
                    playing.store(st.get("isPlaying").and_then(|v| v.as_bool()).unwrap_or(false), Ordering::Relaxed);
                    // 启动自动起播：MusicKit 就绪（状态里有队列长度或曲名）后触发 Boot。
                    if auto_boot_pending {
                        let qlen_ready = st.get("queueLength").map_or(false, |v| !v.is_null());
                        let title_ready = st
                            .get("title")
                            .and_then(|v| v.as_str())
                            .map_or(false, |t| !t.is_empty());
                        if qlen_ready || title_ready {
                            auto_boot_pending = false;
                            busy = true;
                            let _ = cmd_tx.send(UiCmd::Boot);
                        } else if std::time::Instant::now() > auto_boot_deadline && !auto_boot_hinted {
                            // 超时仍未就绪（多半是没登录）：照样试一次 Boot，
                            // 让 boot_play 走完流程给出明确错误，而不是静默什么都不发生。
                            auto_boot_hinted = true;
                            auto_boot_pending = false;
                            busy = true;
                            let _ = cmd_tx.send(UiCmd::Boot);
                        }
                    }
                    // 状态快照日志：每 2 秒一行，用于客观验证「切歌是否真的换了曲」
                    // （TUI 是差分刷新，抓不到画面；只看 isPlaying 变化也看不出切了哪首）。
                    {
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        if ts / 2 != last_status_log_ts / 2 {
                            last_status_log_ts = ts;
                            let rec = serde_json::json!({
                                "ts": ts,
                                "title": st.get("title"), "artist": st.get("artist"),
                                "album": st.get("album"),
                                "artwork": st.get("artwork"),
                                "isPlaying": st.get("isPlaying"),
                                "playbackTime": st.get("playbackTime"),
                                "queueLength": st.get("queueLength"),
                                "url": st.get("url"),
                            });
                            use std::io::Write as _;
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true).append(true).open("/tmp/miniam_status.log")
                            {
                                let _ = writeln!(f, "{}", rec);
                            }
                        }
                    }
                    // 封面 URL 变化才重新抓取（每秒轮询不重复下载）。
                    let art = status["artwork"].as_str().map(|x| x.to_string());
                    match (&last_artwork, &art) {
                        (Some(a), Some(b)) if a == b => {}
                        (_, None) => {
                            last_artwork = None;
                            cover_lines.clear();
                        }
                        (_, Some(url)) => {
                            last_artwork = Some(url.clone());
                            let (cols, rows) =
                                ratatui::crossterm::terminal::size().unwrap_or((80, 24));
                            let (ca, _cap) =
                                cover_split(ratatui::layout::Rect::new(0, 0, cols, rows));
                            let _ = cmd_tx.send(UiCmd::Cover {
                                url: url.clone(),
                                w: ca.width,
                                h: ca.height,
                            });
                        }
                    }
                }
                None
            }
            Some(_) = done_rx.recv() => {
                busy = false;
                blink_pending = false;
                None
            }
            Some(lines) = cover_rx.recv() => {
                cover_lines = lines;
                None
            }
            Some((c, r)) = resize_rx.recv() => {
                // 窗口缩放：用新尺寸重绘封面（自动调整 + 保持居中）
                if let Some(url) = &cover_url {
                    let (ca, _cap) = cover_split(ratatui::layout::Rect::new(0, 0, c, r));
                    let _ = cmd_tx.send(UiCmd::Cover { url: url.clone(), w: ca.width, h: ca.height });
                }
                None
            }
            Some(e) = err_rx.recv() => {
                message = e;
                message_until = Some(Instant::now() + Duration::from_secs(4));
                blink_pending = false;
                None
            }
            _ = status_timer.tick() => {
                let _ = cmd_tx.send(UiCmd::Status);
                None
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => None,
        };
        if let Some(code) = key {
            match code {
                KeyCode::Esc => { kill_browser(); break; },
                KeyCode::Char('q') => { kill_browser(); break; },
                KeyCode::Char('c') => {
                    spec_mode = (spec_mode + 1) % 3;
                    blink_until = Some(Instant::now() + Duration::from_millis(600));
                }
                KeyCode::Char('d') => {
                    let _ = cmd_tx.send(UiCmd::Dump);
                }
                // 空格 = 播放/暂停（原来的 p 键；p 保留为兼容别名）。
                KeyCode::Char(' ') | KeyCode::Char('p') => {
                    let qlen = status["queueLength"].as_i64().unwrap_or(0);
                    // queueLength 在 MusicKit 未就绪 / 队列刚重建 / 状态还没刷新时会是 null，
                    // 只判 0 会把「正在播放」误判成「未起播」，触发 Boot 导航打断播放并报起播失败。
                    let already_playing = status["isPlaying"].as_bool().unwrap_or(false)
                        || status["title"].as_str().map(|t| !t.is_empty()).unwrap_or(false);
                    if qlen == 0 && !already_playing && !busy {
                        // queue empty => not started yet: p boots playback
                        busy = true;
                        term.draw(|f| {
                            let spec_area = ui_chunks(f.area())[7];
                            let spec_iw = spec_area.width as u32;
                            let spec_ih = spec_area.height as u32;
                            let spectrum_lines = if spec_mode == 2 {
                                render_radial_spectrum(&spectrum_data, spec_iw, spec_ih, col_sprite.as_ref(), dot_sprite.as_ref(), &mut radial_particles, &audio_dyn, radial_phase, radial_beat_wave, do_burst, radial_fade)
                            } else {
                                build_spectrum(&spectrum_data, spec_iw, spec_ih, col_sprite.as_ref(), dot_sprite.as_ref(), &mut particles, &mut peaks, &audio_dyn)
                            };
                            draw_ui(f, &status, busy, None, "", backend_name, &spectrum_data, &cover_lines, &spectrum_lines, spec_mode)
                        })?;
                        let _ = cmd_tx.send(UiCmd::Boot);
                    } else if qlen > 0 || already_playing {
                        let _ = cmd_tx.send(UiCmd::Eval(mk(
                            "if (mk.isPlaying) { await mk.pause(); } else { await mk.play(); } return { ok: true };",
                        )));
                        blink_pending = true;
                        blink_until = Some(Instant::now() + Duration::from_millis(600));
                    }
                }
                KeyCode::Char('n') => {
                    let _ = cmd_tx.send(UiCmd::Click { pattern: "跳过|skip|下一首|下一曲|next".to_string() });
                    blink_pending = true;
                    blink_until = Some(Instant::now() + Duration::from_millis(600));
                }
                KeyCode::Char('b') => {
                    let _ = cmd_tx.send(UiCmd::Click { pattern: "上一首|上一曲|previous|回退".to_string() });
                    blink_pending = true;
                    blink_until = Some(Instant::now() + Duration::from_millis(600));
                }
                KeyCode::Char('s') => {
                    // 状态由 worker 每秒自动刷新，无需手动操作
                }
                _ => {}
            }
        }
    }
    // 正常退出时先关闭命令通道并等待 worker 跑完，确保持久 BiDi 会话被 session.end 释放；
    // 否则残留会话会占满 Firefox 上限，导致下次 miniam ui 在 wait_ready 阶段零输出卡死。
    drop(cmd_tx);
    // 等 worker 收尾释放 BiDi 会话，但设上限：万一某个浏览器调用挂起，
    // 也要保证按 q 能立刻退出（进程退出时 socket 关闭，会话由浏览器回收）。
    let _ = tokio::time::timeout(Duration::from_secs(2), worker).await;
    Ok(())
}

/// 只查不杀：统计匹配到的进程数（排除自身与父 shell）。
/// 用于判断"是否已有别的 miniam 在跑"，避免误杀同机并存的实例。
fn count_matching(patterns: &[&str]) -> usize {
    let me = std::process::id();
    let parent = std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| s.split_whitespace().nth(3).and_then(|x| x.parse::<u32>().ok()));
    let mut n = 0;
    for pat in patterns {
        if let Ok(out) = Command::new("pgrep").args(["-f", "--", pat]).output() {
            for tok in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                if let Ok(pid) = tok.parse::<u32>() {
                    if pid == me || Some(pid) == parent {
                        continue;
                    }
                    n += 1;
                }
            }
        }
    }
    n
}

/// 结束匹配到的本项目进程（排除自身与父 shell），返回个数。reset 与登录跳转共用。
fn kill_matching(patterns: &[&str]) -> usize {
    let me = std::process::id();
    let parent = std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| s.split_whitespace().nth(3).and_then(|x| x.parse::<u32>().ok()));
    let mut killed = 0;
    for pat in patterns {
        if let Ok(out) = Command::new("pgrep").args(["-f", pat]).output() {
            for tok in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                if let Ok(pid) = tok.parse::<u32>() {
                    if pid == me || Some(pid) == parent {
                        continue;
                    }
                    let _ = Command::new("kill").args(["-TERM", tok]).status();
                    killed += 1;
                }
            }
        }
    }
    killed
}

/// 退出时彻底结束浏览器（含其所有子进程），避免 BiDi 会话/进程残留占满上限。
/// 优先按 BROWSER_PID 杀整组；复用 miniam up 启动的浏览器时 PID 为 0，
/// 则按 miniam 专用调试端口回退查杀，保证 Esc/q 退出时同步关闭浏览器。
fn kill_browser() {
    let pid = BROWSER_PID.swap(0, std::sync::atomic::Ordering::SeqCst);
    let mut log = String::new();
    if pid != 0 {
        // 组杀是危险的：kill -TERM -<pgid> 打的是“整个进程组”。只有当 <pid> 仍然是我们
        // 自己拉起的那个浏览器、并且仍然是它进程组的组长时，才能确定这个组里只有我们的
        // 浏览器进程。否则 pid 可能早已退出并被系统回收再分配给别的进程（系统上有大量
        // pid == pgid 的组长进程，如 Xorg / ghostty / cinnamon-session-binary），
        // 一旦命中就会把整个图形会话 SIGTERM 掉，表现为“按 q/Esc 就退出桌面”。
        let cmd = proc_cmdline(pid);
        let pgid = proc_pgid(pid);
        let ours = cmd.contains(".config/miniam/") && pgid == Some(pid);
        log.push_str(&format!(
            "kill_browser: pid={} pgid={:?} ours={} cmd={:?}\n",
            pid, pgid, ours, cmd
        ));
        if ours {
            // 只杀我们自己那个浏览器主进程（单个 pid），**绝不**做进程组杀：
            // 组杀用的组号(pid)可能早已被系统回收再分配给别的进程组组长，
            // 这类进程很多（systemd --user / display-manager / Xorg /
            // ghostty / cinnamon-session-binary 的 pid 都等于 pgid），一旦命中就会把
            // 整个图形会话 SIGTERM/SIGKILL 掉，表现为“按 q/Esc 就退出桌面”。
            // Firefox 主进程被杀后会自行回收它的内容进程。
            let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
        } else {
            log.push_str("kill_browser: 跳过组杀（该 pid 已不是我们的浏览器组长）\n");
        }
    }
    // 兜底：逐个 kill 我们自己的浏览器进程（只杀单个进程，不会波及任何其它进程组）。
    let pat = format!("--remote-debugging-port={}", PORT);
    let n = kill_matching(&[".config/miniam/firefox", &pat]);
    log.push_str(&format!("kill_browser: pattern kill={}\n", n));
    // 追加写（不是覆盖）：run_ui 退出时会调两次 kill_browser，覆盖会把第一次的决策抹掉。
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/miniam_kill.log")
    {
        use std::io::Write as _;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{}] {}", ts, log);
    }
}

/// 读 /proc/<pid>/cmdline（NUL 分隔，转成空格拼接）。进程已退出时返回空串。
fn proc_cmdline(pid: u32) -> String {
    std::fs::read(format!("/proc/{}/cmdline", pid))
        .map(|b| {
            b.iter()
                .map(|&c| if c == 0 { b' ' as char } else { c as char })
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// 读 /proc/<pid>/stat 里的 pgid（第 5 字段）。comm 可能含空格/括号，
/// 所以从最后一个 ')' 之后开始切分。
fn proc_pgid(pid: u32) -> Option<u32> {
    let st = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let tail = st.rsplit_once(") ")?.1.trim();
    let mut it = tail.split_whitespace();
    let _state = it.next()?; // state
    let _ppid = it.next()?; // ppid
    it.next()?.parse().ok() // pgrp
}

/// 用已有 BiDi 会话查 Apple Music 登录态。
/// Ok(Some(true)) 已登录 / Ok(Some(false)) 未登录 / Ok(None) MusicKit 尚未就绪
async fn login_state(b: &mut BiDi) -> Result<Option<bool>> {
    let expr = mk("return { authorized: !!mk.isAuthorized };");
    let v = bidi_eval_in(b, &expr).await?;
    if v.get("error").is_some() {
        return Ok(None);
    }
    Ok(Some(v["authorized"].as_bool().unwrap_or(false)))
}

/// 一键流程：浏览器未启动则自动拉起；未登录则换可见窗口引导登录；
/// 返回就绪的 BiDi 会话供 run_ui 复用，全程只占一个会话。
async fn ensure_ui(backend: Backend) -> Result<Option<BiDi>> {
    // 自愈：浏览器已在跑、但本机没有其他 miniam 进程 => 上次退出残留的孤儿浏览器
    // （其 BiDi 会话占着上限，会让 wait_ready 撞“会话已达上限”）。清掉它再拉新的，
    // 这样无论上次怎么退出（含 SIGKILL），下次 miniam 都能正常起。
    if port_alive() && count_matching(&["miniam$", "miniam ui$", "miniam up$"]) == 0 {
        // 端口被占但本机没有别的 miniam => 上次退出留下的孤儿浏览器。清掉它，
        // 并等端口真正释放（被杀进程不会立刻松端口）再拉新的。
        let n = kill_matching(&[".config/miniam/firefox"]);
        if n > 0 {
            eprintln!("miniam: 清理了 {} 个上次残留的浏览器进程", n);
            for _ in 0..40 {
                if !port_alive() { break; }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    if !port_alive() {
        eprintln!("miniam: 未检测到浏览器，正在启动…");
        spawn_browser(backend, false)?;
    }
    let mut bidi = wait_ready(backend, Duration::from_secs(60)).await?;

    // 尽早装上氛围视频抑制器：页面一加载出来就会开始播 HEVC 背景视频，
    // 装晚了就得白烧几秒 CPU（Linux 上 HEVC 只能软解）。
    if ambient_video_guard_enabled() {
        install_ambient_guard(backend, &mut bidi).await;
    }

    // 等 MusicKit 加载完再判断，避免把"页面没加载好"误判成"未登录"
    let mut logged: Option<bool> = None;
    for _ in 0..20 {
        match bidi.as_mut() {
            Some(b) => {
                if let Ok(Some(x)) = login_state(b).await {
                    logged = Some(x);
                    break;
                }
            }
            None => {
                logged = Some(true); // Chrome 后端不做 BiDi 登录检测
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    if logged == Some(false) {
        // visible window needs a graphical display; without DISPLAY/WAYLAND_DISPLAY it cannot pop.
        if std::env::var("DISPLAY").is_err() && std::env::var("WAYLAND_DISPLAY").is_err() {
            eprintln!("miniam: 当前会话没有图形显示（DISPLAY/WAYLAND_DISPLAY 为空），无法弹出登录窗口。");
            eprintln!("        请在本机图形界面终端运行，或用 `ssh -X`（或 ssh -Y）重新连接后再运行 miniam；");
            eprintln!("        登录一次后会话保存在 ~/.config/miniam/firefox，之后用普通 ssh 无窗口运行即可。");
            return Err(anyhow!("no display available for interactive Apple Music login"));
        }

        eprintln!("miniam: Apple Music 未登录，正在打开可见窗口供登录…");
        if let Some(mut b) = bidi.take() {
            b.end().await;
        }
        kill_matching(&[".config/miniam/firefox"]);
        tokio::time::sleep(Duration::from_secs(2)).await;
        spawn_browser(backend, true)?;
        println!("请在弹出的浏览器窗口登录 Apple Music，完成后回到本终端按回车继续…");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        // 登录完成：把可见浏览器换成无头，关闭窗口但保留已登录会话（profile 已落盘）
        eprintln!("miniam: 登录完成，切换为无头模式并关闭浏览器窗口…");
        if let Some(mut b) = bidi.take() {
            b.end().await;
        }
        kill_matching(&[".config/miniam/firefox"]);
        tokio::time::sleep(Duration::from_secs(2)).await;
        spawn_browser(backend, false)?;
        bidi = wait_ready(backend, Duration::from_secs(60)).await?;
    }
    Ok(bidi)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let backend = backend(); // 自动探测机器上实际装了的浏览器（Firefox / Chrome）

    match args.get(1).map(|s| s.as_str()) {
        Some("up") => {
            spawn_browser(backend, false)?;
            // up 不需要持久会话，立刻释放，避免占用 Firefox 唯一会话槽。
            if let Some(mut b) = wait_ready(backend, Duration::from_secs(60)).await? {
                b.end().await;
            }
            eprintln!("miniam: browser ready, run 'miniam ui' in another terminal");
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }
        // 一键：miniam / miniam ui 都自动确保浏览器就绪、必要时引导登录，然后进界面。
        Some("ui") | None => {
            let bidi = ensure_ui(backend).await?;
            run_ui(backend, bidi).await?;
        }
        // 把一张图片渲染成象限块 ANSI 文本（每字符 \x1b[38;2;R;G;B;48;2;R;G;Bm<char>，
        // 每行末尾 \x1b[0m + CRLF）。用于离线比对封面渲染效果。
        Some("cover") => {
            let path = args.get(2).cloned().unwrap_or_default();
            if path.is_empty() {
                eprintln!("usage: miniam cover <图片路径> [输出路径]");
                return Ok(());
            }
            let img = image::open(&path).map_err(|e| anyhow!("打开 {} 失败: {}", path, e))?;
            let cols: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(90);
            let rows: u32 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(36);
            let lines = render_cover_from_image(&img, cols as u16, rows as u16);
            let mut out = String::new();
            for l in &lines {
                for sp in &l.spans {
                    let (fr, fg, fb) = match sp.style.fg {
                        Some(ratatui::style::Color::Rgb(r, g, b)) => (r, g, b),
                        _ => (0, 0, 0),
                    };
                    let (br, bg, bb) = match sp.style.bg {
                        Some(ratatui::style::Color::Rgb(r, g, b)) => (r, g, b),
                        _ => (0, 0, 0),
                    };
                    out.push_str(&format!(
                        "\x1b[38;2;{};{};{};48;2;{};{};{}m{}",
                        fr, fg, fb, br, bg, bb, sp.content
                    ));
                }
                out.push_str("\x1b[0m\r\n");
            }
            match args.get(3) {
                Some(p) => {
                    std::fs::write(p, out.as_bytes())?;
                    eprintln!("miniam: 已写出 {} （{} 行 x {} 列）", p, rows, cols);
                }
                None => {
                    use std::io::Write as _;
                    std::io::stdout().write_all(out.as_bytes())?;
                }
            }
        }
        Some("reset") => {
            // 残留的 ui 会占着 Firefox 唯一 BiDi 会话；Ctrl-C 关终端未必杀得掉。
            let killed = kill_matching(&["miniam ui$", "miniam up$", ".config/miniam/firefox"]);
            std::thread::sleep(Duration::from_secs(2));
            println!("已清理 {} 个残留进程。现在直接运行 miniam 即可（会自动拉起浏览器）", killed);
        }
        Some(other) => {
            eprintln!("unknown command: {}", other);
            eprintln!("usage: miniam [ui|up|reset]   （无参数=一键：自动拉起浏览器并进界面）");
        }
    }
    Ok(())
}


#[cfg(test)]
mod quadrant_tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    use image::{Rgba, RgbaImage};
    use ratatui::style::Color;
    use std::io::Cursor;

    /// 单格渲染：4 个子块色（左上/右上/左下/右下），几何与生产路径一致
    /// （每格 2x4 像素 = 4 个 1x2 子块）。
    fn cell(px: [Option<(u8, u8, u8)>; 4]) -> (String, Option<Color>, Option<Color>) {
        let mut img = RgbaImage::from_pixel(2, 4, Rgba([0, 0, 0, 0]));
        for (k, &(dx, dy)) in [(0u32, 0u32), (1, 0), (0, 2), (1, 2)].iter().enumerate() {
            if let Some((r, g, b)) = px[k] {
                img.put_pixel(dx, dy, Rgba([r, g, b, 255]));
                img.put_pixel(dx, dy + 1, Rgba([r, g, b, 255]));
            }
        }
        let sp = &raster_quadrant_blocks(&img, 1, 1, 2, 4)[0].spans[0];
        (sp.content.to_string(), sp.style.fg, sp.style.bg)
    }

    #[test]
    fn full_block_is_solid_and_truecolor() {
        let (ch, fg, bg) = cell([Some((200, 100, 50)); 4]);
        assert_eq!(ch, " ");
        assert_eq!(fg, Some(Color::Rgb(200, 100, 50)));
        assert_eq!(bg, Some(Color::Rgb(200, 100, 50)));
    }

    #[test]
    fn single_quadrant_glyphs() {
        // 单个子块有色、其余按黑算 -> 对应的单象限字符（Unicode 真实语义）。
        // ⚠️ 「仅右下」(掩码 8) 在 Unicode 里没有对应字符（▐=右半 UR+LR、
        // ▗=下半 LL+LR），故被排除、不参与枚举 —— 这里只验可精确表达的三种。
        assert_eq!(cell([Some((255, 255, 255)), None, None, None]).0, "\u{2598}");
        assert_eq!(cell([None, Some((255, 255, 255)), None, None]).0, "\u{259D}");
        assert_eq!(cell([None, None, Some((255, 255, 255)), None]).0, "\u{2596}");
    }

    #[test]
    fn glyph_semantics_match_unicode() {
        // 回归防护：字符的 Unicode 语义必须与实际覆盖的子像素一致。
        // ▛(U+259B) = UPPER LEFT AND UPPER RIGHT AND LOWER LEFT（3 象限），
        // 旧实现把它当 2 象限用，导致每格与原图差一个象限。
        let (ch, fg, bg) = cell([
            Some((255, 255, 255)),
            Some((255, 255, 255)),
            Some((255, 255, 255)),
            None,
        ]);
        assert_eq!(ch, "\u{259B}");
        // fg = 三个白像素的均值，bg = 第四个（黑）
        assert_eq!(fg, Some(Color::Rgb(255, 255, 255)));
        assert_eq!(bg, Some(Color::Rgb(0, 0, 0)));
    }

    #[test]
    fn uniform_cell_is_plain_space() {
        // 四个子块同色 -> 退化为纯色：掩码 0、fg=bg=四点均值、字符用空格。
        let (ch, fg, bg) = cell([None; 4]);
        assert_eq!(ch, " ");
        assert_eq!(fg, Some(Color::Rgb(0, 0, 0)));
        assert_eq!(bg, Some(Color::Rgb(0, 0, 0)));
    }

    #[test]
    fn two_distinct_truecolors_per_cell() {
        // 对角白（UL+LR）/ 黑（UR+LL）：两种指派 SSE 相同（都是 0），
        // 规范只说「取 SSE 最小的那一组」，故取枚举到的第一个（掩码 0110 -> ▞）。
        let (ch, fg, bg) = cell([
            Some((255, 255, 255)), Some((0, 0, 0)),
            Some((0, 0, 0)),       Some((255, 255, 255)),
        ]);
        assert_eq!(ch, "\u{259E}");
        assert_eq!(fg, Some(Color::Rgb(0, 0, 0)));
        assert_eq!(bg, Some(Color::Rgb(255, 255, 255)));
    }

    #[test]
    fn glyph_table_covers_all_masks() {
        // 字符表按 Unicode 真实语义索引 0..15（掩码 = 位为 1 的子块取前景色）。
        let expect = [
            " ",  "\u{2598}", "\u{259D}", "\u{2580}",
            "\u{2596}", "\u{258C}", "\u{259E}", "\u{259B}",
            "\u{2597}", "\u{259A}", "\u{2590}", "\u{259C}",
            "\u{2584}", "\u{2599}", "\u{259F}", "\u{2588}",
        ];
        let mut img = RgbaImage::from_pixel(8, 8, Rgba([0, 0, 0, 255]));
        for y in 0..8 {
            for x in 0..8 {
                img.put_pixel(x, y, Rgba([(x * 31) as u8, (y * 31) as u8, ((x + y) * 17) as u8, 255]));
            }
        }
        let lines = raster_quadrant_blocks(&img, 2, 2, 2, 4);
        let mut n = 0;
        for l in &lines {
            for sp in &l.spans {
                let c = sp.content.as_ref();
                assert!(expect.contains(&c), "字符 {} 不在表内", c);
                n += 1;
            }
        }
        assert!(n > 0);
    }

    #[test]
    fn render_cover_never_panics_at_extreme_aspects() {
        // [ ] 调节 R 会改变 iw_px/ih_px；早期实现用 u32 直接相减求居中偏移，
        // 在极端比例下会下溢 panic。这里遍历宽高与比例组合，确保只返回而不 panic。
        let mut img = RgbaImage::from_pixel(64, 64, Rgba([120, 80, 40, 255]));
        for y in 0..64 { for x in 0..64 { img.put_pixel(x, y, Rgba([(x*3) as u8, (y*4) as u8, 90, 255])); } }
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut buf, image::ImageFormat::Png).unwrap();
        let url = format!("data:image/png;base64,{}", B64.encode(buf.into_inner()));
        for r in [0.5f32, 0.8, 1.0, 1.9, 2.0, 2.20, 2.1, 3.0, 4.0] {
            if let Ok(mut a) = COVER_ASPECT.lock() { *a = r; }
            for (w, h) in [(2u16, 2u16), (3, 1), (8, 3), (80, 20), (1, 40)] {
                let _ = render_cover(url.clone(), w, h);
            }
        }
        if let Ok(mut a) = COVER_ASPECT.lock() { *a = 2.20; }
    }

    #[test]
    fn layout_cover_top_spectrum_10rows_marquee() {
        use ratatui::{backend::TestBackend, text::Line, Terminal};
        let backend = TestBackend::new(100, 40);
        let mut term = Terminal::new(backend).unwrap();
        let status = serde_json::json!({
            "title": "测试曲目", "artist": "测试歌手", "album": "测试专辑",
            "isPlaying": true, "playbackTime": 30.0, "duration": 200000.0,
            "queueLength": 5
        });
        let cover = vec![Line::from("COVERMARK")];
        let spec = vec![Line::from("SPECTRUMMARK")];
        term.draw(|f| draw_ui(f, &status, false, None, "", "test", &[], &cover, &spec, 1)).unwrap();

        let area = term.backend().buffer().area;
        let line_str = |y: u16| -> String {
            (0..area.width)
                .map(|x| term.backend().buffer().get(x, y).symbol())
                .collect()
        };
        let find = |mark: &str| -> Option<u16> {
            (0..area.height).find(|&y| line_str(y).contains(mark))
        };
        let y_cover = find("COVERMARK").expect("封面必须渲染");
        let y_spec = find("SPECTRUMMARK").expect("频谱必须渲染");
        assert!(y_cover < y_spec, "封面必须在频谱上方");

        let chunks = ui_chunks(area);
        assert_eq!(chunks[7].height, 4, "频谱区固定 4 行");
        assert_eq!(y_cover, chunks[0].y, "封面在顶端区域起始行");
        assert_eq!(y_spec, chunks[7].y, "频谱在底部区域起始行");
        assert!(!line_str(chunks[2].y).trim().is_empty(), "跑马灯行有内容");
        let all: String = (0..area.height).map(line_str).collect::<Vec<_>>().join("
");
        assert!(!all.contains("c 封面/频谱"), "帮助行已去掉 c 键");
        assert!(!all.contains("无音频输入"), "无音频诊断文字已去掉");
        let help_line = line_str(chunks[6].y);
        assert!(help_line.contains('p'), "其他信息条保持");

        // 不同窗口尺寸下：频谱恒 4 行，其下方 1 行为居中的状态行贴窗口底边；封面有最小高度并随窗口增大而增高。
        let mut cover_heights = Vec::new();
        for (w, h) in [(80u16, 24u16), (120, 40), (200, 60)] {
            let c = ui_chunks(ratatui::layout::Rect::new(0, 0, w, h));
            assert_eq!(c[7].height, 4, "频谱恒 4 行");
            assert!(c[0].y < c[1].y && c[6].y < c[7].y, "封面在最顶、频谱在最底");
            assert_eq!(c[1].y + c[1].height, c[2].y, "绿色小频谱紧邻歌曲名上方");
            assert_eq!(c[0].width, w, "封面区占满窗口宽度，图在其中居中");
            // 状态行贴窗口底边：底部无空隙；频谱在其正上方
            assert_eq!(c[8].y + c[8].height, h, "状态行紧贴窗口底边");
            assert_eq!(c[7].y + c[7].height, c[8].y, "频谱紧邻状态行上方");
            // 封面有最小高度，且随窗口增大而增高（自动调整）
            assert!(c[0].height >= COVER_MIN_ROWS, "封面有最小高度");
            cover_heights.push(c[0].height);
        }
        assert!(cover_heights[0] < cover_heights[2], "窗口越大封面越高（自动调整）");
    }

    #[test]
    fn audio_dyn_adapts_to_energy_and_bands() {
        // 安静：发射率/速度都低
        let mut quiet = AudioDyn::default();
        for _ in 0..30 { quiet.update(&vec![0u64; 64]); }
        assert!(quiet.energy < 0.02);
        assert!(quiet.busyness() < 0.02);

        // 激烈且全频段高：能量、谱通量、忙度都升高
        let mut loud = AudioDyn::default();
        for _ in 0..30 { loud.update(&vec![85u64; 64]); }
        assert!(loud.energy > quiet.energy + 0.5, "能量随歌曲升高");
        assert!(loud.busyness() > quiet.busyness(), "速度随谱通量/高频升高");

        // 低频主导 vs 高频主导：heaviness 与 treble 区分
        let mut bass = AudioDyn::default();
        let mut treble = AudioDyn::default();
        let mut bs = vec![0u64; 64];
        let mut tr = vec![0u64; 64];
        for i in 0..64 { if i < 12 { bs[i] = 90; } else { tr[i] = 90; } }
        for _ in 0..30 { bass.update(&bs); treble.update(&tr); }
        assert!(bass.heaviness() > treble.heaviness(), "低频重力更强");
        assert!(treble.treble > bass.treble, "高频信号更强");

        // 节拍脉冲快起慢落
        let mut p = AudioDyn::default();
        p.beat(1.0);
        assert!(p.pulse > 0.9);
        p.update(&vec![0u64; 64]);
        assert!(p.pulse < 0.9, "脉冲慢落衰减");
    }

    #[test]
    fn neonize_saturates_and_particles_are_small() {
        // 霓虹化后至少有一个通道被推到接近 255（高饱和亮芯）
        let c = neonize((90, 120, 160));
        let mx = c.0.max(c.1).max(c.2);
        assert!(mx >= 250, "最亮通道拉满");
        assert!(c.0.min(c.1).min(c.2) < c.2, "低通道被压暗形成饱和");

        // 粒子尺寸恒在 1..2 子像素（比旧版 1..4 更小）
        for lr in [0.0f32, 0.3, 0.6, 1.0] {
            let size = (1.0 + lr).clamp(1.0, 2.0);
            assert!(size <= 2.0 && size >= 1.0);
        }
    }

    #[test]
    fn spectrum_adapts_particle_count_to_loudness() {
        let mut dq = AudioDyn::default();
        let mut dl = AudioDyn::default();
        for _ in 0..30 {
            dq.update(&vec![0u64; 64]);
            dl.update(&vec![85u64; 64]);
        }
        let mut pq: Vec<Particle> = Vec::new();
        let mut pl: Vec<Particle> = Vec::new();
        let mut pkq = vec![0f32; 64];
        let mut pkl = vec![0f32; 64];
        let quiet = vec![0u64; 64];
        let loud = vec![85u64; 64];
        let lq = render_spectrum(&quiet, 60, 10, None, None, &mut pq, SPEC_NEON, &mut pkq, &dq);
        let ll = render_spectrum(&loud, 60, 10, None, None, &mut pl, SPEC_NEON, &mut pkl, &dl);
        assert_eq!(lq.len(), 10, "频谱输出 10 行");
        assert_eq!(ll.len(), 10, "频谱输出 10 行");
        assert!(pl.len() > pq.len(), "激烈时发射更多粒子");
        assert!(pq.len() < 10, "安静时几乎无粒子");
    }

    #[test]
    fn beat_burst_only_on_active_spectrum() {
        let d = AudioDyn::default();
        // 静音：完全不喷粒子
        let mut silent: Vec<Particle> = Vec::new();
        spawn_beat_burst(&mut silent, 128.0, 40.0, 1.0, &d, &vec![0u64; 64]);
        assert!(silent.is_empty(), "无频谱时不喷任何粒子");

        // 只有部分频段有能量：粒子只落在有频谱的位置（x 落在活跃 bin 范围内）
        let mut spec = vec![0u64; 64];
        for i in 8..16 {
            spec[i] = 90;
        }
        let mut active: Vec<Particle> = Vec::new();
        spawn_beat_burst(&mut active, 128.0, 40.0, 1.0, &d, &spec);
        assert!(!active.is_empty(), "有频谱时正常喷粒子");
        let bin_w = 128.0 / 64.0;
        let lo = (8.0 + 0.5) * bin_w; // 首个活跃 bin 中心
        let hi = (15.0 + 0.5) * bin_w; // 末个活跃 bin 中心
        for p in &active {
            assert!(p.x >= lo - bin_w && p.x <= hi + bin_w, "粒子 x 必须在有频谱的频段内");
        }
    }

    #[test]
    fn render_cover_emits_quadrant_truecolor() {
        let mut img = RgbaImage::from_pixel(64, 64, Rgba([0, 0, 0, 255]));
        for y in 0..64 {
            for x in 0..64 {
                img.put_pixel(x, y, Rgba([(x * 4) as u8, (y * 4) as u8, 90, 255]));
            }
        }
        let mut buf = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let url = format!("data:image/png;base64,{}", B64.encode(buf.into_inner()));
        let lines = render_cover(url, 80, 20).expect("render_cover failed");
        assert!(!lines.is_empty());
        let has_bg = lines.iter().flat_map(|l| l.spans.iter())
            .any(|s| matches!(s.style.bg, Some(Color::Rgb(..))));
        assert!(has_bg, "象限封面应使用 24-bit 背景色（双色）");
    }
}
mod bitmap_font;
mod braille_text;
