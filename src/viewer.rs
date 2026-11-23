use crate::db::Store;
use crate::i18n::t;

/// 默认滚轮缩放步进。
pub const DEFAULT_ZOOM_STEP: f32 = 1.25;
/// 最小滚轮缩放步进。
pub const MIN_ZOOM_STEP: f32 = 1.05;
/// 最大滚轮缩放步进。
pub const MAX_ZOOM_STEP: f32 = 2.0;

/// 预览画布双击行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DcAction {
    /// 在适应窗口与实际大小之间切换。
    #[default]
    FitToggle,
    /// 始终显示实际像素大小。
    ActualSize,
    /// 交给系统关联程序打开。
    OpenWith,
}

impl DcAction {
    /// 双击行为的可选顺序。
    pub const ALL: &'static [Self] = &[Self::FitToggle, Self::ActualSize, Self::OpenWith];

    /// 设置键值。
    pub fn key(self) -> &'static str {
        match self {
            Self::FitToggle => "fit_toggle",
            Self::ActualSize => "actual_size",
            Self::OpenWith => "open_with",
        }
    }

    /// 从设置键值解析。
    pub fn from_key(s: &str) -> Option<Self> {
        Self::ALL.into_iter().copied().find(|a| a.key() == s)
    }

    /// 界面文案。
    pub fn label(self) -> &'static str {
        t(match self {
            Self::FitToggle => "双击时:适应窗口 ↔ 实际大小",
            Self::ActualSize => "双击时:始终实际大小",
            Self::OpenWith => "双击时:用系统程序打开",
        })
    }
}

/// 预览画布底色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewerBg {
    /// 跟随当前界面主题。
    #[default]
    Theme,
    /// 深黑底色。
    Dark,
    /// 浅白底色。
    Light,
}

impl ViewerBg {
    /// 底色的可选顺序。
    pub const ALL: &'static [Self] = &[Self::Theme, Self::Dark, Self::Light];

    /// 设置键值。
    pub fn key(self) -> &'static str {
        match self {
            Self::Theme => "theme",
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    /// 从设置键值解析。
    pub fn from_key(s: &str) -> Option<Self> {
        Self::ALL.into_iter().copied().find(|bg| bg.key() == s)
    }

    /// 界面文案。
    pub fn label(self) -> &'static str {
        t(match self {
            Self::Theme => "跟随主题",
            Self::Dark => "深黑",
            Self::Light => "浅白",
        })
    }
}

/// 查看器偏好。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewerPrefs {
    /// 双击行为。
    pub double_click: DcAction,
    /// 画布底色。
    pub bg: ViewerBg,
    /// 滚轮缩放步进。
    pub zoom_step: f32,
}

impl Default for ViewerPrefs {
    fn default() -> Self {
        Self {
            double_click: DcAction::FitToggle,
            bg: ViewerBg::Theme,
            zoom_step: DEFAULT_ZOOM_STEP,
        }
    }
}

impl ViewerPrefs {
    /// 从设置载入,坏值回退默认。
    pub fn load(store: &Store) -> Self {
        let double_click = store
            .get_setting("viewer_dc")
            .and_then(|v| DcAction::from_key(&v))
            .unwrap_or_default();
        let bg = store
            .get_setting("viewer_bg")
            .and_then(|v| ViewerBg::from_key(&v))
            .unwrap_or_default();
        let zoom_step = store
            .get_setting("viewer_zoom_step")
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| v.is_finite() && (MIN_ZOOM_STEP..=MAX_ZOOM_STEP).contains(v))
            .unwrap_or(DEFAULT_ZOOM_STEP);
        Self {
            double_click,
            bg,
            zoom_step,
        }
    }

    /// 保存三项查看器设置。
    pub fn save(&self, store: &Store) {
        let _ = store.set_setting("viewer_dc", self.double_click.key());
        let _ = store.set_setting("viewer_bg", self.bg.key());
        let _ = store.set_setting("viewer_zoom_step", &self.zoom_step.to_string());
    }
}

/// 按步进缩放并夹紧范围。
pub fn next_zoom(cur: f32, step: f32, dir: i8, max: f32) -> f32 {
    let floor = 0.05;
    let cur = if cur.is_finite() { cur.max(floor) } else { 1.0 };
    let max = if max.is_finite() { max.max(floor) } else { 8.0 };
    let step = if step.is_finite() {
        step.max(1.0)
    } else {
        DEFAULT_ZOOM_STEP
    };
    let next = if dir < 0 {
        cur / step
    } else if dir == 0 {
        cur
    } else {
        cur * step
    };
    next.clamp(floor, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;

    #[test]
    fn prefs_setting_keys_roundtrip() {
        for action in DcAction::ALL {
            assert_eq!(DcAction::from_key(action.key()), Some(*action));
        }
        for bg in ViewerBg::ALL {
            assert_eq!(ViewerBg::from_key(bg.key()), Some(*bg));
        }
        assert_eq!(DcAction::from_key("bogus"), None);
        assert_eq!(ViewerBg::from_key("bogus"), None);
    }

    #[test]
    fn prefs_persist_and_default_to_current_behavior() {
        let store = Store::open_mem().unwrap();
        let prefs = ViewerPrefs::load(&store);
        assert_eq!(prefs.double_click, DcAction::FitToggle);
        assert_eq!(prefs.bg, ViewerBg::Theme);
        assert_eq!(prefs.zoom_step, 1.25);
        let changed = ViewerPrefs {
            double_click: DcAction::ActualSize,
            bg: ViewerBg::Dark,
            zoom_step: 1.6,
        };
        changed.save(&store);
        assert_eq!(ViewerPrefs::load(&store), changed);
        let _ = store.set_setting("viewer_zoom_step", "abc");
        assert_eq!(ViewerPrefs::load(&store).zoom_step, 1.25);
    }

    #[test]
    fn next_zoom_respects_step_floor_and_cap() {
        assert!((next_zoom(1.0, 1.25, 1, 8.0) - 1.25).abs() < 1e-6);
        assert!((next_zoom(1.0, 1.25, -1, 8.0) - 0.8).abs() < 1e-6);
        assert_eq!(next_zoom(7.5, 2.0, 1, 8.0), 8.0);
        assert!(next_zoom(0.02, 2.0, -1, 8.0) >= 0.05);
    }
}
