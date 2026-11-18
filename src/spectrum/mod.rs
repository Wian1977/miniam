//! 频谱插件框架（第一阶段：静态注册；第二阶段将支持运行时目录热加载）
//!
//! 五种内置频谱（spec_mode 0/1/2/3/4）：
//!   0 经典（classic） / 1 霓虹粒子（neon） / 2 放射（radial） / 3 流体（fluid） / 4 彩虹摩尔纹（moire）
//!
//! 每个插件是一个独立文件，实现 [`SpectrumPlugin`] trait。主循环持有一个
//! `Box<dyn SpectrumPlugin>`，按 spec_mode 通过 [`make_plugin`] 创建/切换。
//!
//! 第二阶段热加载：约定运行时目录 `./spectrum_plugins/`（或 XDG config 下），
//! miniam 启动/运行中监控该目录，用 libloading dlopen 后填充注册表，
//! 达到「放入文件无需重启即载入」的效果（届时 make_plugin 改为从注册表取）。

use crate::AudioDyn;
use ratatui::text::Line;

/// 每帧由主循环算好、传给各插件的派生量。
pub struct SpectrumCtx {
    pub phase: f32,
    pub beat_wave: Option<f32>,
    pub do_burst: Option<f32>,
    pub fade: f32,
    pub neon: (u8, u8, u8),
    /// 当前封面的**两个渐变端点色**（起点偏暗饱和、终点偏亮），供频谱配色随封面自适应。
    /// None = 无封面，走默认配色。两个色相相距 ≥40°，保证是渐变而非单色。
    pub cover_grad: Option<((u8, u8, u8), (u8, u8, u8))>,
}

/// 频谱插件接口：输入频谱 + 派生上下文，输出 braille 栅格的行。
pub trait SpectrumPlugin: Send {
    /// 人类可读名称（用于 UI / 调试）。
    fn name(&self) -> &'static str { "unnamed" }
    /// 渲染一帧。插件自行持有跨帧持久状态（粒子、水位等）。
    fn render(
        &mut self,
        spectrum: &[u64],
        cols: u32,
        rows: u32,
        dyn_state: &AudioDyn,
        ctx: &SpectrumCtx,
    ) -> Vec<Line<'static>>;

}

pub mod classic;
pub mod neon;
pub mod radial;
pub mod fluid;
pub mod moire;

/// 第一阶段：按 spec_mode 静态创建对应插件。
/// 第二阶段将改为从目录加载的注册表按名/索引取。
pub fn make_plugin(mode: u8) -> Box<dyn SpectrumPlugin> {
    match mode {
        0 => Box::new(classic::ClassicPlugin::default()),
        1 => Box::new(neon::NeonPlugin::default()),
        2 => Box::new(radial::RadialPlugin::default()),
        3 => Box::new(fluid::FluidPlugin::default()),
        4 => Box::new(moire::MoirePlugin::default()),
        _ => Box::new(neon::NeonPlugin::default()),
    }
}

// 第二阶段（尚未实现）：从指定目录加载 `.so` 插件。
// pub fn load_plugins_from_dir(_dir: &std::path::Path) -> Vec<Box<dyn SpectrumPlugin>> {
//     Vec::new()
// }
