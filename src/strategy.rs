//! 高覆盖遍历策略：动作候选生成 + 未探索优先的加权选择 + 状态图 + 覆盖率统计。
//!
//! 思路参考 Fastbot：
//!  1. 以「Activity + 可交互控件身份集合」做状态指纹（结构优先，不受动态文本干扰），
//!     去重并记录访问次数；
//!  2. 每个状态维护「已尝试动作集合」，优先挑选从未尝试过的动作；
//!  3. 全局维护「控件是否被操作过」，未操作过的控件权重放大，驱动覆盖率提升；
//!  4. 状态无未尝试动作时按权重随机（探索 vs 利用），连续卡在同一状态则触发回溯（返回键）。

use crate::model::{Action, Hierarchy, Rect, SwipeDir, TargetInfo};
use crate::util::hash64;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct ExplorerConfig {
    pub package: String,
    pub text_pool: Vec<String>,
    pub max_same_state: usize,
    pub enable_long_click: bool,
    pub enable_input: bool,
    pub enable_horizontal_swipe: bool,
    /// 状态指纹取法，默认结构优先
    pub state_mode: StateMode,
}

impl Default for ExplorerConfig {
    fn default() -> Self {
        Self {
            package: String::new(),
            text_pool: default_text_pool(),
            max_same_state: 8,
            enable_long_click: true,
            enable_input: true,
            enable_horizontal_swipe: true,
            state_mode: StateMode::default(),
        }
    }
}

/// 状态指纹的取法
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StateMode {
    /// 结构优先（默认）：只看可交互控件的身份集合，动态文本不影响判定
    #[default]
    Structural,
    /// 内容精确：界面全部文本都算进指纹。动态文本多时状态会爆炸，仅用于对照排查
    Exact,
}

impl StateMode {
    pub fn as_str(self) -> &'static str {
        match self {
            StateMode::Structural => "structural",
            StateMode::Exact => "exact",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "structural" | "struct" | "s" => Some(StateMode::Structural),
            "exact" | "content" | "e" => Some(StateMode::Exact),
            _ => None,
        }
    }
}

pub fn default_text_pool() -> Vec<String> {
    vec![
        "test", "123456", "hello", "abc123", "user01", "2026", "a", "android", "password", "10086",
    ]
    .into_iter()
    .map(|s| s.to_string())
    .collect()
}

struct Candidate {
    action: Action,
    weight: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateRecord {
    pub id: u64,
    pub activity: String,
    pub visits: u32,
    pub first_step: usize,
    pub node_count: usize,
    pub interactive_count: usize,
    pub tried_count: usize,
    /// 该状态的「控件签名」：状态里**可交互控件**的稳定指纹集合（排序去重）。
    ///
    /// 状态 id 是整棵可见树的指纹，内容一变就换 id —— 在抖音这种无限推荐流上，
    /// 每刷一次都是「新状态」，页面清单会退化成一条状态链。
    /// 这组 key 是页面的语义骨架（按钮/输入框/Tab 的身份），内容帧之间基本不变，
    /// 下游（L1 页面层）据此可以把同一页面的多个状态归并成一个页面。
    #[serde(default)]
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityRecord {
    pub name: String,
    pub visits: u32,
    pub first_step: usize,
    pub nodes: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStat {
    pub key: String,
    pub label: String,
    pub class: String,
    pub resource_id: String,
    pub activity: String,
    pub seen: u32,
    pub touched: u32,
    /// 是否是「可交互目标」（见 `UiNode::coverage_target`）。
    /// 同一 key 只要出现过一次可交互形态就算可交互。
    #[serde(default)]
    pub interactive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub from: u64,
    pub to: u64,
    pub action: String,
    pub step: usize,
}

pub struct Explorer {
    cfg: ExplorerConfig,
    rng: StdRng,
    states: HashMap<u64, StateRecord>,
    pub state_order: Vec<u64>,
    activities: BTreeMap<String, ActivityRecord>,
    nodes: HashMap<String, NodeStat>,
    tried: HashMap<u64, HashSet<String>>,
    pub transitions: Vec<Transition>,
    last_state: Option<u64>,
    same_streak: usize,
}

impl Explorer {
    pub fn new(cfg: ExplorerConfig, seed: u64) -> Self {
        Self {
            cfg,
            rng: StdRng::seed_from_u64(seed),
            states: HashMap::new(),
            state_order: Vec::new(),
            activities: BTreeMap::new(),
            nodes: HashMap::new(),
            tried: HashMap::new(),
            transitions: Vec::new(),
            last_state: None,
            same_streak: 0,
        }
    }

    // -------------------------------------------------- 状态观察

    /// 记录一次观测，返回 (state_id, 是否新状态)
    pub fn observe(&mut self, h: &Hierarchy, activity: &str, step: usize) -> (u64, bool) {
        // 全树遍历只做这一次：状态指纹、控件登记、Activity 登记共用同一份可见节点
        let visible = h.visible_nodes();
        let screen = h.screen();
        let sig = match self.cfg.state_mode {
            StateMode::Structural => Hierarchy::struct_sig_with(activity, &screen, &visible),
            StateMode::Exact => Hierarchy::state_sig_with(activity, &screen, &visible),
        };
        let id = hash64(&sig);
        let is_new = !self.states.contains_key(&id);

        // 覆盖率：登记所有可见控件
        for n in &visible {
            if !self.cfg.package.is_empty()
                && !n.package.is_empty()
                && n.package != self.cfg.package
            {
                continue; // 系统/其它应用的控件不计入被测应用覆盖率
            }
            let key = n.key();
            let is_target = n.coverage_target();
            let record = self.nodes.entry(key.clone()).or_insert_with(|| NodeStat {
                key: key.clone(),
                label: n.display(),
                class: n.short_class(),
                resource_id: n.resource_id.clone(),
                activity: activity.to_string(),
                seen: 0,
                touched: 0,
                interactive: is_target,
            });
            record.seen += 1;
            record.interactive |= is_target;
            if record.activity.is_empty() {
                record.activity = activity.to_string();
            }
        }

        let interactive_count = visible.iter().filter(|n| n.interactive()).count();

        let st = self.states.entry(id).or_insert_with(|| StateRecord {
            id,
            activity: activity.to_string(),
            visits: 0,
            first_step: step,
            node_count: h.node_count(),
            interactive_count,
            tried_count: 0,
            keys: {
                let mut ks: Vec<String> = visible
                    .iter()
                    .filter(|n| n.coverage_target())
                    .map(|n| n.key())
                    .collect();
                ks.sort();
                ks.dedup();
                ks
            },
        });
        st.visits += 1;
        if is_new {
            self.state_order.push(id);
        }

        let act = self
            .activities
            .entry(activity.to_string())
            .or_insert_with(|| ActivityRecord {
                name: activity.to_string(),
                visits: 0,
                first_step: step,
                nodes: HashSet::new(),
            });
        act.visits += 1;
        if !self.cfg.package.is_empty()
            && !activity.is_empty()
            && activity.starts_with(&self.cfg.package)
        {
            for n in &visible {
                if !n.package.is_empty() && n.package != self.cfg.package {
                    continue;
                }
                act.nodes.insert(n.key());
            }
        }

        // 连续相同状态计数（卡死检测）
        if self.last_state == Some(id) {
            self.same_streak += 1;
        } else {
            self.same_streak = 0;
        }
        self.last_state = Some(id);

        (id, is_new)
    }

    pub fn state_count(&self) -> usize {
        self.states.len()
    }

    /// 按首次出现顺序返回状态记录。
    ///
    /// 主要供会话落盘使用：把每个状态的控件签名一并写出去，
    /// 下游才能把「同一页面的不同内容帧」归并成页面（见 `StateRecord::keys`）。
    pub fn states_in_order(&self) -> Vec<StateRecord> {
        self.state_order
            .iter()
            .filter_map(|id| self.states.get(id).cloned())
            .collect()
    }

    pub fn same_streak(&self) -> usize {
        self.same_streak
    }

    pub fn is_stuck(&self) -> bool {
        self.same_streak >= self.cfg.max_same_state
    }

    // -------------------------------------------------- 动作生成

    fn candidates(&mut self, h: &Hierarchy) -> Vec<Candidate> {
        let screen = h.screen();
        let screen_area = screen.area().max(1) as f32;
        let mut out: Vec<Candidate> = Vec::new();

        for n in h.visible_nodes() {
            if !n.enabled {
                continue;
            }
            let ratio = n.bounds.area() as f32 / screen_area;
            if ratio > 0.92 {
                continue; // 几乎全屏的容器，点击没有区分度
            }
            if n.bounds.w() < 4 || n.bounds.h() < 4 {
                continue;
            }
            let point = match n.click_point(&screen) {
                Some(p) => p,
                None => continue,
            };

            let foreign = !self.cfg.package.is_empty()
                && !n.package.is_empty()
                && n.package != self.cfg.package;
            let mut base = if foreign { 0.4f32 } else { 1.0f32 };

            // 全局未被操作过的控件放大权重——这是提升覆盖率的主要驱动力
            let touched = self.nodes.get(&n.key()).map(|s| s.touched).unwrap_or(0);
            if touched == 0 {
                base *= 3.0;
            } else if touched <= 2 {
                base *= 1.4;
            } else {
                base *= 0.6;
            }

            let target = TargetInfo::from_node(n);

            if n.clickable || n.checkable || n.is_edit_text() {
                out.push(Candidate {
                    action: Action::Click {
                        target: target.clone(),
                        point,
                    },
                    weight: 10.0 * base,
                });
            } else if n.is_leaf_with_content() {
                // 文本叶子节点：自身不可点击，但点击通常会冒泡到父容器
                out.push(Candidate {
                    action: Action::Click {
                        target: target.clone(),
                        point,
                    },
                    weight: 4.0 * base,
                });
            }

            if self.cfg.enable_long_click && n.long_clickable {
                out.push(Candidate {
                    action: Action::LongClick {
                        target: target.clone(),
                        point,
                    },
                    weight: 3.0 * base,
                });
            }

            if self.cfg.enable_input && n.is_edit_text() && !self.cfg.text_pool.is_empty() {
                let text = self
                    .cfg
                    .text_pool
                    .choose(&mut self.rng)
                    .cloned()
                    .unwrap_or_else(|| "test".to_string());
                out.push(Candidate {
                    action: Action::Input {
                        target: target.clone(),
                        point,
                        text,
                    },
                    weight: 5.0 * base,
                });
            }

            if n.scrollable {
                for (dir, w) in [
                    (SwipeDir::Down, 6.0),
                    (SwipeDir::Up, 6.0),
                    (SwipeDir::Left, 2.0),
                    (SwipeDir::Right, 2.0),
                ] {
                    if !self.cfg.enable_horizontal_swipe
                        && matches!(dir, SwipeDir::Left | SwipeDir::Right)
                    {
                        continue;
                    }
                    let (from, to) = swipe_points(&n.bounds.clamp_inside(&screen), dir);
                    out.push(Candidate {
                        action: Action::Swipe {
                            target: Some(target.clone()),
                            from,
                            to,
                            dir,
                            duration_ms: 320,
                        },
                        weight: w * base,
                    });
                }
            }
        }

        // 找不到任何控件目标时，允许在屏幕中央滑一下（例如游戏/自绘界面）
        if out.is_empty() {
            let (from, to) = swipe_points(&screen.shrink(0.2, 0.2), SwipeDir::Up);
            out.push(Candidate {
                action: Action::Swipe {
                    target: None,
                    from,
                    to,
                    dir: SwipeDir::Up,
                    duration_ms: 320,
                },
                weight: 1.0,
            });
        }
        out
    }

    /// 挑选下一个动作；返回 None 表示当前状态的动作已穷尽（调用方应触发回溯）
    pub fn choose(&mut self, state_id: u64, h: &Hierarchy) -> Option<Action> {
        let cands = self.candidates(h);
        if cands.is_empty() {
            return None;
        }
        let tried_set = self.tried.get(&state_id);
        let fresh: Vec<&Candidate> = cands
            .iter()
            .filter(|c| !tried_set.is_some_and(|t| t.contains(&c.action.action_key())))
            .collect();

        let chosen = if !fresh.is_empty() {
            // 未探索优先：在未尝试过的动作里按权重随机
            weighted_pick(&fresh, &mut self.rng).clone()
        } else {
            // 已穷尽：随机重放，可能触发新的状态分支
            let all: Vec<&Candidate> = cands.iter().collect();
            weighted_pick(&all, &mut self.rng).clone()
        };

        let key = chosen.action_key();
        let set = self.tried.entry(state_id).or_default();
        set.insert(key);
        let n = set.len();
        if let Some(st) = self.states.get_mut(&state_id) {
            st.tried_count = n;
        }
        Some(chosen)
    }

    /// 记录状态迁移（from --action--> to）
    pub fn note_transition(&mut self, from: u64, action_key: String, to: u64, step: usize) {
        self.transitions.push(Transition {
            from,
            to,
            action: action_key,
            step,
        });
    }

    pub fn take_transitions(&mut self) -> Vec<Transition> {
        std::mem::take(&mut self.transitions)
    }

    pub fn mark_touched(&mut self, key: &str) {
        if let Some(stat) = self.nodes.get_mut(key) {
            stat.touched += 1;
        }
    }

    pub fn reset_streak(&mut self) {
        self.same_streak = 0;
    }

    // -------------------------------------------------- 覆盖率

    pub fn coverage(&self) -> CoverageReport {
        let mut activities: Vec<ActivityCoverage> = self
            .activities
            .values()
            .filter(|a| self.cfg.package.is_empty() || a.name.starts_with(&self.cfg.package))
            .map(|a| {
                let mut total = 0usize;
                let mut touched = 0usize;
                let mut i_total = 0usize;
                let mut i_touched = 0usize;
                for k in &a.nodes {
                    let stat = self.nodes.get(k);
                    let was_touched = stat.map(|s| s.touched > 0).unwrap_or(false);
                    total += 1;
                    if was_touched {
                        touched += 1;
                    }
                    if stat.map(|s| s.interactive).unwrap_or(false) {
                        i_total += 1;
                        if was_touched {
                            i_touched += 1;
                        }
                    }
                }
                ActivityCoverage {
                    name: a.name.clone(),
                    visits: a.visits,
                    first_step: a.first_step,
                    nodes_seen: total,
                    nodes_touched: touched,
                    coverage: ratio(touched, total),
                    interactive_seen: i_total,
                    interactive_touched: i_touched,
                    interactive_coverage: ratio(i_touched, i_total),
                }
            })
            .collect();
        activities.sort_by_key(|a| std::cmp::Reverse(a.nodes_seen));

        let total_nodes = self.nodes.len();
        let touched_nodes = self.nodes.values().filter(|n| n.touched > 0).count();
        let interactive_nodes = self.nodes.values().filter(|n| n.interactive).count();
        let interactive_touched = self
            .nodes
            .values()
            .filter(|n| n.interactive && n.touched > 0)
            .count();

        let mut untouched: Vec<NodeStat> = self
            .nodes
            .values()
            .filter(|n| n.touched == 0)
            .cloned()
            .collect();
        // 可交互的排前面（那才是"还差什么没点到"），其次按出现次数
        untouched.sort_by_key(|n| (std::cmp::Reverse(n.interactive), std::cmp::Reverse(n.seen)));
        let untouched_interactive = untouched.iter().filter(|n| n.interactive).count();

        CoverageReport {
            activity_count: activities.len(),
            state_count: self.states.len(),
            total_nodes,
            touched_nodes,
            node_coverage: ratio(touched_nodes, total_nodes),
            interactive_nodes,
            interactive_touched,
            interactive_coverage: ratio(interactive_touched, interactive_nodes),
            activities,
            untouched_interactive,
            untouched_sample: untouched.into_iter().take(50).collect(),
        }
    }
}

/// 求比例，分母为 0 时返回 0（避免 NaN 进 JSON）
fn ratio(part: usize, whole: usize) -> f32 {
    if whole == 0 {
        0.0
    } else {
        part as f32 / whole as f32
    }
}

fn weighted_pick<'a>(cands: &[&'a Candidate], rng: &mut StdRng) -> &'a Action {
    let total: f32 = cands.iter().map(|c| c.weight).sum();
    if total <= 0.0 {
        return &cands[rng.gen_range(0..cands.len())].action;
    }
    let mut r: f32 = rng.gen_range(0.0..total);
    for c in cands {
        r -= c.weight;
        if r <= 0.0 {
            return &c.action;
        }
    }
    &cands[cands.len() - 1].action
}

/// 在矩形内按方向生成滑动起止点（纯几何，便于执行与标注保持一致）
pub fn swipe_points(r: &Rect, dir: SwipeDir) -> ((i32, i32), (i32, i32)) {
    let inner = r.shrink(0.15, 0.15);
    match dir {
        SwipeDir::Up => (
            (inner.cx(), (inner.y2 * 3 + inner.y1) / 4),
            (inner.cx(), (inner.y1 * 3 + inner.y2) / 4),
        ),
        SwipeDir::Down => (
            (inner.cx(), (inner.y1 * 3 + inner.y2) / 4),
            (inner.cx(), (inner.y2 * 3 + inner.y1) / 4),
        ),
        SwipeDir::Left => (
            ((inner.x2 * 3 + inner.x1) / 4, inner.cy()),
            ((inner.x1 * 3 + inner.x2) / 4, inner.cy()),
        ),
        SwipeDir::Right => (
            ((inner.x1 * 3 + inner.x2) / 4, inner.cy()),
            ((inner.x2 * 3 + inner.x1) / 4, inner.cy()),
        ),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityCoverage {
    pub name: String,
    pub visits: u32,
    pub first_step: usize,
    pub nodes_seen: usize,
    pub nodes_touched: usize,
    pub coverage: f32,
    /// 可交互控件口径（见 `CoverageReport::interactive_coverage`）
    #[serde(default)]
    pub interactive_seen: usize,
    #[serde(default)]
    pub interactive_touched: usize,
    #[serde(default)]
    pub interactive_coverage: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CoverageReport {
    pub activity_count: usize,
    pub state_count: usize,
    /// 全部可见节点（含不可交互的纯布局容器）
    pub total_nodes: usize,
    pub touched_nodes: usize,
    pub node_coverage: f32,
    /// 可交互控件（含文本叶子节点），这才是覆盖率的主指标 ——
    /// 纯容器永远点不到，算进分母只会让数字永远上不去
    #[serde(default)]
    pub interactive_nodes: usize,
    #[serde(default)]
    pub interactive_touched: usize,
    #[serde(default)]
    pub interactive_coverage: f32,
    pub activities: Vec<ActivityCoverage>,
    /// 未覆盖样本中可交互的个数（报告里优先展示这一批）
    #[serde(default)]
    pub untouched_interactive: usize,
    pub untouched_sample: Vec<NodeStat>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dump::parse_hierarchy;

    const SAMPLE: &str = r#"<hierarchy rotation="0" width="1080" height="2400">
      <node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.demo" bounds="[0,0][1080,2400]">
        <node index="0" text="列表" resource-id="com.demo:id/list" class="android.widget.ListView" package="com.demo" scrollable="true" bounds="[0,100][1080,2200]">
          <node index="0" text="条目A" resource-id="com.demo:id/item_title" class="android.widget.TextView" package="com.demo" clickable="true" bounds="[0,120][1080,220]" />
          <node index="1" text="条目B" resource-id="com.demo:id/item_title" class="android.widget.TextView" package="com.demo" clickable="true" bounds="[0,240][1080,340]" />
        </node>
        <node index="1" text="" resource-id="com.demo:id/et" class="android.widget.EditText" package="com.demo" clickable="true" long-clickable="true" bounds="[40,2250][1040,2350]" />
      </node>
    </hierarchy>"#;

    fn cfg() -> ExplorerConfig {
        ExplorerConfig {
            package: "com.demo".into(),
            ..Default::default()
        }
    }

    fn sig(h: &Hierarchy, activity: &str) -> String {
        Hierarchy::state_sig_with(activity, &h.screen(), &h.visible_nodes())
    }

    #[test]
    fn state_fingerprint_stable_and_content_sensitive() {
        let h1 = parse_hierarchy(SAMPLE).unwrap();
        let h2 = parse_hierarchy(SAMPLE).unwrap();
        assert_eq!(sig(&h1, "com.demo/.Main"), sig(&h2, "com.demo/.Main"));

        let changed = SAMPLE.replace("条目B", "条目C");
        let h3 = parse_hierarchy(&changed).unwrap();
        assert_ne!(sig(&h1, "com.demo/.Main"), sig(&h3, "com.demo/.Main"));
    }

    #[test]
    fn generates_and_exhausts_actions() {
        let h = parse_hierarchy(SAMPLE).unwrap();
        let mut ex = Explorer::new(cfg(), 42);
        let (sid, is_new) = ex.observe(&h, "com.demo/.Main", 0);
        assert!(is_new);

        let mut seen = HashSet::new();
        for _ in 0..80 {
            if let Some(a) = ex.choose(sid, &h) {
                seen.insert(a.action_key());
            }
        }
        // 至少覆盖到：两个条目点击 + 输入框点击/长按/输入 + 列表上下滑动
        assert!(
            seen.iter()
                .any(|k| k.starts_with("click#com.demo:id/item_title")),
            "{:?}",
            seen
        );
        assert!(
            seen.iter()
                .any(|k| k.starts_with("swipe#com.demo:id/list#up")),
            "{:?}",
            seen
        );
        assert!(
            seen.iter()
                .any(|k| k.starts_with("swipe#com.demo:id/list#down")),
            "{:?}",
            seen
        );
        assert!(seen.iter().any(|k| k.starts_with("input#")), "{:?}", seen);
        assert!(
            seen.iter().any(|k| k.starts_with("long_click#")),
            "{:?}",
            seen
        );
    }

    #[test]
    fn coverage_grows_after_touch() {
        let h = parse_hierarchy(SAMPLE).unwrap();
        let mut ex = Explorer::new(cfg(), 7);
        let (sid, _) = ex.observe(&h, "com.demo/.Main", 0);
        let c0 = ex.coverage();
        assert_eq!(c0.touched_nodes, 0);
        assert!(c0.total_nodes > 0);

        let a = ex.choose(sid, &h).unwrap();
        // 与 runner 主循环一致：动作执行后立刻把目标控件计为已触碰
        if let Some(k) = a.target_key() {
            ex.mark_touched(&k);
        }
        ex.note_transition(sid, a.action_key(), sid, 1);
        let c1 = ex.coverage();
        assert!(c1.touched_nodes >= 1);
        assert!(c1.node_coverage > 0.0);
        assert_eq!(c1.activity_count, 1);
    }

    #[test]
    fn swipe_points_sane() {
        let r = Rect::new(0, 0, 1000, 1000);
        let (f, t) = swipe_points(&r, SwipeDir::Up);
        assert!(f.1 > t.1);
        let (f, t) = swipe_points(&r, SwipeDir::Left);
        assert!(f.0 > t.0);
    }

    /// 带一个「时钟」TextView 的界面：内容每帧都在变，结构不变
    fn clock_hierarchy(time: &str) -> String {
        format!(
            r#"<hierarchy rotation="0" width="1080" height="2400">
      <node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.demo" bounds="[0,0][1080,2400]">
        <node index="0" text="{time}" resource-id="" class="android.widget.TextView" package="com.demo" bounds="[40,40][300,100]" />
        <node index="1" text="确定" resource-id="com.demo:id/ok" class="android.widget.Button" package="com.demo" clickable="true" bounds="[40,200][300,280]" />
      </node>
    </hierarchy>"#
        )
    }

    #[test]
    fn structural_sig_resists_dynamic_text() {
        let times = ["09:01", "09:02", "09:03"];
        let structs: Vec<String> = times
            .iter()
            .map(|t| {
                let h = parse_hierarchy(&clock_hierarchy(t)).unwrap();
                Hierarchy::struct_sig_with("com.demo/.Main", &h.screen(), &h.visible_nodes())
            })
            .collect();
        assert!(
            structs.iter().all(|s| *s == structs[0]),
            "时钟文本变化不应改变结构签名: {structs:?}"
        );

        // 对照组：完整签名对文本敏感——这正是它会让状态爆炸的原因
        let fulls: Vec<String> = times
            .iter()
            .map(|t| {
                let h = parse_hierarchy(&clock_hierarchy(t)).unwrap();
                Hierarchy::state_sig_with("com.demo/.Main", &h.screen(), &h.visible_nodes())
            })
            .collect();
        assert!(
            fulls.windows(2).all(|w| w[0] != w[1]),
            "完整签名应当对文本敏感: {fulls:?}"
        );
    }

    #[test]
    fn structural_mode_avoids_state_explosion() {
        // 时钟每步都在走：结构优先只应产生 1 个状态；精确模式会一路爆成 5 个，
        // 结果是同一页面上的控件被反复当成「未探索动作」点击。
        let count = |mode: StateMode| {
            let mut ex = Explorer::new(
                ExplorerConfig {
                    package: "com.demo".into(),
                    state_mode: mode,
                    ..Default::default()
                },
                1,
            );
            for t in ["09:01", "09:02", "09:03", "09:04", "09:05"] {
                let h = parse_hierarchy(&clock_hierarchy(t)).unwrap();
                ex.observe(&h, "com.demo/.Main", 0);
            }
            ex.state_count()
        };
        assert_eq!(count(StateMode::Structural), 1);
        assert_eq!(count(StateMode::Exact), 5);
    }

    #[test]
    fn coverage_counts_only_interactive_targets() {
        let h = parse_hierarchy(SAMPLE).unwrap();
        let mut ex = Explorer::new(cfg(), 3);
        ex.observe(&h, "com.demo/.Main", 0);
        let c = ex.coverage();
        // 根 FrameLayout 既不可交互也没有内容，属于纯容器，不该算进可交互分母
        assert!(
            c.interactive_nodes < c.total_nodes,
            "可交互目标({}) 应少于全部节点({})",
            c.interactive_nodes,
            c.total_nodes
        );
        // SAMPLE 里的可交互目标：list(scrollable) + item_title ×2(同 key 合并) + et
        assert_eq!(c.interactive_nodes, 3);
        assert_eq!(c.interactive_touched, 0);
        assert_eq!(c.interactive_coverage, 0.0);
        // 未覆盖样本里可交互的应排在前面
        assert!(c
            .untouched_sample
            .first()
            .map(|n| n.interactive)
            .unwrap_or(false));
        assert_eq!(c.untouched_interactive, 3);
    }
}
