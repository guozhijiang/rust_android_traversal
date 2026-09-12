//! 控件树与动作的数据模型。

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------- 矩形

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Rect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl Rect {
    pub fn new(x1: i32, y1: i32, x2: i32, y2: i32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    /// 解析 uiautomator 的 bounds：`[0,72][1080,2400]`
    pub fn parse(s: &str) -> Option<Rect> {
        let mut nums: Vec<i32> = Vec::with_capacity(4);
        let mut cur = String::new();
        let mut neg = false;
        for c in s.chars() {
            match c {
                '-' => {
                    if cur.is_empty() {
                        neg = true;
                    }
                }
                '0'..='9' => cur.push(c),
                _ => {
                    if !cur.is_empty() {
                        let mut v: i32 = cur.parse().ok()?;
                        if neg {
                            v = -v;
                        }
                        nums.push(v);
                        cur.clear();
                        neg = false;
                    }
                }
            }
        }
        if !cur.is_empty() {
            let mut v: i32 = cur.parse().ok()?;
            if neg {
                v = -v;
            }
            nums.push(v);
        }
        if nums.len() >= 4 {
            Some(Rect::new(nums[0], nums[1], nums[2], nums[3]))
        } else {
            None
        }
    }

    pub fn w(&self) -> i32 {
        (self.x2 - self.x1).max(0)
    }
    pub fn h(&self) -> i32 {
        (self.y2 - self.y1).max(0)
    }
    pub fn cx(&self) -> i32 {
        (self.x1 + self.x2) / 2
    }
    pub fn cy(&self) -> i32 {
        (self.y1 + self.y2) / 2
    }
    pub fn area(&self) -> i64 {
        self.w() as i64 * self.h() as i64
    }
    pub fn valid(&self) -> bool {
        self.w() > 0 && self.h() > 0
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x1 && x < self.x2 && y >= self.y1 && y < self.y2
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        Rect::new(
            self.x1.max(o.x1),
            self.y1.max(o.y1),
            self.x2.min(o.x2),
            self.y2.min(o.y2),
        )
    }
    /// 按比例内缩，用于 swipe 起止点不要贴边
    pub fn shrink(&self, fx: f32, fy: f32) -> Rect {
        let dx = ((self.w() as f32 * fx) / 2.0) as i32;
        let dy = ((self.h() as f32 * fy) / 2.0) as i32;
        Rect::new(self.x1 + dx, self.y1 + dy, self.x2 - dx, self.y2 - dy)
    }
    pub fn clamp_inside(&self, o: &Rect) -> Rect {
        Rect::new(
            self.x1.clamp(o.x1, o.x2),
            self.y1.clamp(o.y1, o.y2),
            self.x2.clamp(o.x1, o.x2),
            self.y2.clamp(o.y1, o.y2),
        )
    }

    /// 可视面积占自身面积的比例（被其它窗口遮挡/在屏幕外时 < 1）
    pub fn visible_ratio(&self, screen: &Rect) -> f32 {
        if !self.valid() {
            return 0.0;
        }
        let inter = self.intersect(screen);
        inter.area() as f32 / self.area() as f32
    }
}

impl fmt::Display for Rect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{},{}][{},{}]", self.x1, self.y1, self.x2, self.y2)
    }
}

// ---------------------------------------------------------------- 控件节点

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UiNode {
    #[serde(default)]
    pub index: i32,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub resource_id: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub package: String,
    #[serde(default)]
    pub content_desc: String,
    #[serde(default)]
    pub bounds: Rect,
    #[serde(default)]
    pub clickable: bool,
    #[serde(default)]
    pub long_clickable: bool,
    #[serde(default)]
    pub scrollable: bool,
    #[serde(default)]
    pub checkable: bool,
    #[serde(default)]
    pub checked: bool,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub selected: bool,
    #[serde(default)]
    pub password: bool,
    #[serde(default)]
    pub children: Vec<UiNode>,
}

impl UiNode {
    pub fn short_class(&self) -> String {
        self.class
            .rsplit('.')
            .next()
            .unwrap_or(&self.class)
            .to_string()
    }

    /// 简短可读标识，用于日志与报告
    pub fn display(&self) -> String {
        if !self.text.is_empty() {
            return crate::util::truncate(&self.text, 24);
        }
        if !self.content_desc.is_empty() {
            return crate::util::truncate(&self.content_desc, 24);
        }
        if !self.resource_id.is_empty() {
            return self
                .resource_id
                .rsplit('/')
                .next()
                .unwrap_or(&self.resource_id)
                .to_string();
        }
        self.short_class()
    }

    pub fn is_edit_text(&self) -> bool {
        self.class.contains("EditText") || self.class.contains("AutoCompleteTextView")
    }

    /// 是否值得作为交互目标
    pub fn interactive(&self) -> bool {
        self.clickable
            || self.long_clickable
            || self.scrollable
            || self.checkable
            || self.is_edit_text()
    }

    /// 是否计入「可交互控件覆盖率」的分母。
    ///
    /// 比 [`interactive`](Self::interactive) 宽一点：把有文本/描述的叶子节点也算进来
    /// （这类节点自身不可点击，但点击会冒泡到父容器，属于有效操作目标）；
    /// 但排除 FrameLayout 这类既不可交互、又没有任何内容的纯布局容器 ——
    /// 它们永远不会被点击，算进分母只会让覆盖率永远到不了 100%，指标失真。
    pub fn coverage_target(&self) -> bool {
        self.interactive() || self.is_leaf_with_content()
    }

    /// 有内容但自身不可点击的叶子节点（例如列表项容器内的文本），
    /// 点击通常会冒泡到父级 clickable，是提高覆盖率的重要补充。
    pub fn is_leaf_with_content(&self) -> bool {
        self.children.is_empty() && (!self.text.is_empty() || !self.content_desc.is_empty())
    }

    /// 状态指纹专用的稳定身份：**不含文本**。
    ///
    /// 与 [`key`](Self::key) 的区别在于兜底分支：`key()` 在无 resource-id 时会退到
    /// `class + 文本`，于是时钟、电量、倒计时这类动态文本会让身份每帧都变，
    /// 状态指纹跟着抖。这里退到 `class + 位置`，位置不变则身份不变。
    pub fn struct_key(&self) -> String {
        if !self.resource_id.is_empty() {
            return self.resource_id.clone();
        }
        format!("{}#{}", self.short_class(), self.bounds)
    }

    /// 稳定身份：优先 resource-id，其次 class+文本，最后 class+位置
    pub fn key(&self) -> String {
        if !self.resource_id.is_empty() {
            return self.resource_id.clone();
        }
        let label = if !self.text.is_empty() {
            self.text.clone()
        } else {
            self.content_desc.clone()
        };
        if !label.is_empty() {
            return format!("{}#{}", self.short_class(), label);
        }
        format!("{}#{}", self.short_class(), self.bounds)
    }

    /// 参与状态指纹的“内容签名”（含文本与选中态，内容变化即可视作新状态）
    pub fn content_sig(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.key(),
            self.text,
            self.content_desc,
            self.checked
        )
    }

    /// 先序遍历（含自身）
    pub fn walk(&self) -> Vec<&UiNode> {
        let mut out = Vec::new();
        fn rec<'a>(n: &'a UiNode, out: &mut Vec<&'a UiNode>) {
            out.push(n);
            for c in &n.children {
                rec(c, out);
            }
        }
        rec(self, &mut out);
        out
    }

    /// 屏幕内的可点坐标（取自身与屏幕交集的中心）
    pub fn click_point(&self, screen: &Rect) -> Option<(i32, i32)> {
        let r = self.bounds.intersect(screen);
        if !r.valid() {
            return None;
        }
        let mut p = (r.cx(), r.cy());
        // 中心不可靠（例如超长文本）时退回到靠上的位置
        if !self.bounds.contains(p.0, p.1) {
            p = (r.cx(), (r.y1 + 8).clamp(r.y1, r.y2 - 1));
        }
        Some(p)
    }
}

// ---------------------------------------------------------------- 控件树

#[derive(Debug, Clone, Default)]
pub struct Hierarchy {
    pub root: Option<UiNode>,
    pub width: i32,
    pub height: i32,
    pub rotation: i32,
}

impl Hierarchy {
    pub fn screen(&self) -> Rect {
        Rect::new(0, 0, self.width.max(1), self.height.max(1))
    }

    pub fn nodes(&self) -> Vec<&UiNode> {
        match &self.root {
            Some(r) => r.walk(),
            None => Vec::new(),
        }
    }

    /// 节点总数（不分配中间 Vec 的计数）
    pub fn node_count(&self) -> usize {
        fn rec(n: &UiNode) -> usize {
            1 + n.children.iter().map(rec).sum::<usize>()
        }
        self.root.as_ref().map(rec).unwrap_or(0)
    }

    /// 可见（在屏幕内且有面积）的节点
    pub fn visible_nodes(&self) -> Vec<&UiNode> {
        let screen = self.screen();
        self.nodes()
            .into_iter()
            .filter(|n| n.bounds.valid() && n.bounds.visible_ratio(&screen) > 0.2)
            .collect()
    }

    /// 状态指纹：交互控件的内容签名集合 + 全屏文本集合。
    /// `visible` 需为 `visible_nodes()` 的结果，屏幕尺寸参与签名以避免旋转/分屏造成的坐标误解
    /// 结构签名（状态指纹的结构部分）：只由**可交互控件的身份**构成，不含文本内容。
    ///
    /// 用它做状态去重可以抵抗动态文本——时间、电量、进度百分比、推荐流内容这类
    /// 每屏都在变的东西，否则每一步都会被判成「新状态」，后果是：
    ///   1. 同一页面上的控件被反复当作「未探索动作」点击，白白消耗步数；
    ///   2. 卡死检测（连续 N 步停留在同一状态）永远无法触发，死循环时出不来。
    pub fn struct_sig_with(activity: &str, screen: &Rect, visible: &[&UiNode]) -> String {
        let mut parts: Vec<String> = Vec::new();
        for n in visible {
            if n.coverage_target() {
                parts.push(format!("{}|{}", n.struct_key(), n.checked));
            }
        }
        parts.sort();
        parts.dedup();
        format!(
            "{}|{}x{}|{}",
            activity,
            screen.w(),
            screen.h(),
            parts.join(";")
        )
    }

    /// 完整内容签名：在结构签名之外还算进所有文本（旧行为，见 `StateMode::Exact`）
    pub fn state_sig_with(activity: &str, screen: &Rect, visible: &[&UiNode]) -> String {
        let mut inter: Vec<String> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        for n in visible {
            if n.interactive() || n.is_leaf_with_content() {
                inter.push(n.content_sig());
            }
            if !n.text.is_empty() {
                texts.push(n.text.clone());
            }
        }
        inter.sort();
        inter.dedup();
        texts.sort();
        texts.dedup();
        // 屏幕尺寸参与签名，避免旋转/分屏造成的坐标误解
        format!(
            "{}|{}x{}|{}|{}",
            activity,
            screen.w(),
            screen.h(),
            inter.join(";"),
            texts.join(";")
        )
    }
}

// ---------------------------------------------------------------- 动作

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TargetInfo {
    pub key: String,
    pub class: String,
    pub resource_id: String,
    pub text: String,
    pub content_desc: String,
    pub bounds: Rect,
    pub label: String,
}

impl TargetInfo {
    pub fn from_node(n: &UiNode) -> Self {
        Self {
            key: n.key(),
            class: n.short_class(),
            resource_id: n.resource_id.clone(),
            text: crate::util::truncate(&n.text, 40),
            content_desc: crate::util::truncate(&n.content_desc, 40),
            bounds: n.bounds,
            label: n.display(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwipeDir {
    Up,
    Down,
    Left,
    Right,
}

impl SwipeDir {
    pub fn as_str(&self) -> &'static str {
        match self {
            SwipeDir::Up => "up",
            SwipeDir::Down => "down",
            SwipeDir::Left => "left",
            SwipeDir::Right => "right",
        }
    }
    /// 中文名，报告里展示
    pub fn cn(&self) -> &'static str {
        match self {
            SwipeDir::Up => "上滑",
            SwipeDir::Down => "下滑",
            SwipeDir::Left => "左滑",
            SwipeDir::Right => "右滑",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Action {
    Launch {
        component: String,
    },
    Back,
    Key {
        code: i32,
        name: String,
    },
    Click {
        target: TargetInfo,
        point: (i32, i32),
    },
    LongClick {
        target: TargetInfo,
        point: (i32, i32),
    },
    Swipe {
        target: Option<TargetInfo>,
        from: (i32, i32),
        to: (i32, i32),
        dir: SwipeDir,
        duration_ms: u32,
    },
    Input {
        target: TargetInfo,
        point: (i32, i32),
        text: String,
    },
}

impl Action {
    /// 动作类型（英文短名，用于文件名/样式）
    pub fn kind(&self) -> &'static str {
        match self {
            Action::Launch { .. } => "launch",
            Action::Back => "back",
            Action::Key { .. } => "key",
            Action::Click { .. } => "click",
            Action::LongClick { .. } => "long_click",
            Action::Swipe { .. } => "swipe",
            Action::Input { .. } => "input",
        }
    }

    /// 中文类型名，报告展示
    pub fn kind_cn(&self) -> &'static str {
        match self {
            Action::Launch { .. } => "启动应用",
            Action::Back => "返回键",
            Action::Key { .. } => "按键",
            Action::Click { .. } => "点击",
            Action::LongClick { .. } => "长按",
            Action::Swipe { .. } => "滑动",
            Action::Input { .. } => "输入文本",
        }
    }

    pub fn label(&self) -> String {
        match self {
            Action::Launch { component } => format!("启动 {}", component),
            Action::Back => "返回键".to_string(),
            Action::Key { name, .. } => format!("按键 {}", name),
            Action::Click { target, point } => {
                format!("点击 {} @({},{})", target.label, point.0, point.1)
            }
            Action::LongClick { target, point } => {
                format!("长按 {} @({},{})", target.label, point.0, point.1)
            }
            Action::Swipe {
                target,
                from,
                to,
                dir,
                ..
            } => {
                let who = target
                    .as_ref()
                    .map(|t| t.label.clone())
                    .unwrap_or_else(|| "屏幕".to_string());
                format!(
                    "{} {} ({},{})->({},{})",
                    dir.cn(),
                    who,
                    from.0,
                    from.1,
                    to.0,
                    to.1
                )
            }
            Action::Input { target, text, .. } => format!("向 {} 输入 “{}”", target.label, text),
        }
    }

    /// 同一状态下区分不同动作用的键
    pub fn action_key(&self) -> String {
        match self {
            Action::Launch { component } => format!("launch#{}", component),
            Action::Back => "back".to_string(),
            Action::Key { code, .. } => format!("key#{}", code),
            Action::Click { target, .. } => format!("click#{}", target.key),
            Action::LongClick { target, .. } => format!("long_click#{}", target.key),
            Action::Swipe { target, dir, .. } => format!(
                "swipe#{}#{}",
                target
                    .as_ref()
                    .map(|t| t.key.clone())
                    .unwrap_or_else(|| "screen".to_string()),
                dir.as_str()
            ),
            Action::Input { target, text, .. } => format!("input#{}#{}", target.key, text),
        }
    }

    /// 被操作的控件（用于覆盖率统计）
    pub fn target_key(&self) -> Option<String> {
        match self {
            Action::Click { target, .. }
            | Action::LongClick { target, .. }
            | Action::Input { target, .. } => Some(target.key.clone()),
            Action::Swipe {
                target: Some(t), ..
            } => Some(t.key.clone()),
            _ => None,
        }
    }
}
