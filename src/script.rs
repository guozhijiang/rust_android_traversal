//! 用例 IR 执行器（`atraverse script`）——「需求 → 用例 IR → 真机执行 → 用例级报告」的最后一环。
//!
//! 与 `run`（遍历模式）的区别在于**产物**：遍历模式的产物是覆盖率，
//! 这里的产物是逐条用例的 `PASS / FAIL / BLOCKED`。
//!
//! 三条不可动摇的约束（来自本方案的设计文档）：
//!
//! 1. **定位器校验闸门不可省略**：用例里每个元素引用都必须回查 L2 元素注册表，
//!    未命中一律 `blocked` 且**不执行**。这是防 LLM 幻觉的关键工程手段 ——
//!    不拦的话，生成出来的用例 100% 跑不起来，而失败现象看起来像「App 有 bug」。
//! 2. **存在性断言不得依赖位置 / 序号定位器**：位置只能回答「该点在哪」，
//!    回答不了「控件在不在」；`class_index` 更糟 —— 同 class 的节点一大把
//!    （一个详情页几十个 View），它几乎总能匹配到某个无关节点，于是断言永远通过。
//!    **假阳性比 FAIL 危险得多**：FAIL 会有人去查，假的 PASS 会让没测到的用例看起来是绿的。
//! 3. **断言只能来自用例（L3 规则层）**，执行器不许现场发明预期值，
//!    否则测试会自我实现（AI 编的预期和 AI 编的结果互相印证）。
//!
//! 用例 IR 里 `steps` / `assertions` 的 `locator` 用**元素注册表 id** 引用元素，
//! 运行时才把该元素的**分级定位器链**展开成真机操作 ——
//! 所以用例不写坐标、不写选择器，定位策略的演进不需要改用例。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::device::{self, Device};
use crate::dump;
use crate::model::{Action, Hierarchy, SwipeDir, TargetInfo, UiNode};
use crate::monitor::{Incident, LogcatMonitor};
use crate::runner;
use crate::util;

/// 断言默认等待时长（会一直重试 dump 直到超时）
const DEFAULT_ASSERT_TIMEOUT_MS: u64 = 4000;
/// 断言重试间隔
const RETRY_INTERVAL: Duration = Duration::from_millis(1200);
/// 滑动默认时长
const SWIPE_MS: u32 = 400;

// ==================================================================== L2 注册表

#[derive(Debug, Clone, Deserialize)]
pub struct LocatorDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
    #[serde(default)]
    pub confidence: f32,
}

impl LocatorDef {
    /// 是否属于「语义定位器」——能回答「这个控件在不在」。
    ///
    /// 位置（`bounds_center`）与序号（`class_index`）不算：它们只能给一个坐标，
    /// 给不了身份。存在性断言必须只认语义定位器。
    pub fn is_semantic(&self) -> bool {
        !matches!(self.kind.as_str(), "bounds_center" | "class_index")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ElementDef {
    pub id: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub semantic: String,
    #[serde(default)]
    pub locators: Vec<LocatorDef>,
}

impl ElementDef {
    /// 报告与日志里显示的名字：优先语义标注，退回元素 id
    pub fn label(&self) -> String {
        if self.semantic.is_empty() {
            self.id.clone()
        } else {
            self.semantic.clone()
        }
    }
}

#[derive(Debug, Deserialize)]
struct ElementFile {
    #[serde(default)]
    elements: Vec<ElementDef>,
}

/// L2 元素注册表：`<kb>/elements/*.yaml` 合并而来
#[derive(Debug, Default)]
pub struct Registry {
    by_id: BTreeMap<String, ElementDef>,
    files: usize,
}

impl Registry {
    pub fn load(kb: &Path) -> Result<Self> {
        let dir = kb.join("elements");
        if !dir.is_dir() {
            bail!(
                "元素注册表目录不存在: {}（L2 由 tools/snapshot.py 从设备 dump 生成）",
                dir.display()
            );
        }
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("无法读取 {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "yaml" || x == "yml"))
            .collect();
        paths.sort();

        let mut reg = Registry::default();
        for p in paths {
            let text =
                std::fs::read_to_string(&p).with_context(|| format!("无法读取 {}", p.display()))?;
            let f: ElementFile = serde_yaml::from_str(&text)
                .with_context(|| format!("解析 {} 失败", p.display()))?;
            reg.files += 1;
            for e in f.elements {
                reg.by_id.entry(e.id.clone()).or_insert(e);
            }
        }
        if reg.is_empty() {
            bail!("{} 下没有解析到任何元素", dir.display());
        }
        Ok(reg)
    }

    pub fn get(&self, id: &str) -> Option<&ElementDef> {
        self.by_id.get(id)
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn files(&self) -> usize {
        self.files
    }
}

// ==================================================================== 用例 IR

/// 用例里对元素的引用：优先按注册表 id，也允许内联定位器
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ElemRef {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Step {
    #[serde(default)]
    pub n: Option<u32>,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub locator: Option<ElemRef>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub component: Option<String>,
    #[serde(default)]
    pub code: Option<i32>,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub desc: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Assertion {
    #[serde(default)]
    pub n: Option<u32>,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub expect: Option<String>,
    #[serde(default)]
    pub locator: Option<ElemRef>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub desc: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Trace {
    #[serde(default)]
    pub requirement: String,
    #[serde(default)]
    pub pattern: String,
    #[serde(default)]
    pub dimension: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Case {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub priority: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub trace: Trace,
    /// 预热步骤：跑在 `steps` 之前。冷启动慢的页面（推荐流、首屏加载）
    /// 用显式预热代替「靠 timeout 硬等」—— 那会把环境问题伪装成用例失败。
    #[serde(default)]
    pub setup: Vec<Step>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub assertions: Vec<Assertion>,
    #[serde(default)]
    pub teardown: Vec<Step>,
}

/// 所有已知的步骤动作
const KNOWN_ACTIONS: [&str; 8] = [
    "click",
    "long_click",
    "input",
    "swipe",
    "back",
    "key",
    "launch",
    "wait",
];

/// 需要页面上定位元素才能执行的动作
const LOCATOR_ACTIONS: [&str; 4] = ["click", "long_click", "input", "swipe"];

/// 所有已知的断言类型
const KNOWN_ASSERTIONS: [&str; 7] = [
    "text_visible",
    "text_contains",
    "text_equals",
    "element_exists",
    "element_absent",
    "activity_is",
    "no_crash",
];

/// 需要页面上定位元素的断言
const LOCATOR_ASSERTIONS: [&str; 2] = ["element_exists", "element_absent"];

/// 需要 `expect` 文本的断言
const EXPECT_ASSERTIONS: [&str; 4] = [
    "text_visible",
    "text_contains",
    "text_equals",
    "activity_is",
];

// ==================================================================== 校验闸门

#[derive(Debug, Clone, Serialize)]
pub struct GateIssue {
    /// 位置，例如 `step 1` / `assert 2`
    pub location: String,
    pub element: String,
    pub reason: String,
}

/// 定位器校验闸门：把用例里每个元素引用回查 L2，未命中即拦下。
///
/// 返回的 issue 为空才允许执行。**这一步是把「生成即失败」变成「生成即可执行」的分水岭。**
pub fn gate_case(case: &Case, reg: &Registry) -> Vec<GateIssue> {
    let mut issues = Vec::new();

    // 没有任何断言 ⇒ 这条「用例」没有判据，跑过去也证明不了什么。
    // 这种用例在报告里必然是绿的，但它什么都没验证 —— 属于最隐蔽的假阳性来源。
    if case.assertions.is_empty() {
        issues.push(GateIssue {
            location: "用例".to_string(),
            element: String::new(),
            reason: "没有任何断言：没有判据的用例不能算测试（断言应来自 L3 规则层）".to_string(),
        });
    }

    check_gate_steps(&case.setup, "setup ", reg, &mut issues);
    check_gate_steps(&case.steps, "step ", reg, &mut issues);

    for (i, a) in case.assertions.iter().enumerate() {
        let at = format!("assert {}", a.n.unwrap_or(i as u32 + 1));
        if !KNOWN_ASSERTIONS.contains(&a.kind.as_str()) {
            issues.push(GateIssue {
                location: at,
                element: String::new(),
                reason: format!("未知断言类型 {:?}", a.kind),
            });
            continue;
        }
        if EXPECT_ASSERTIONS.contains(&a.kind.as_str())
            && a.expect.as_deref().unwrap_or("").is_empty()
        {
            issues.push(GateIssue {
                location: at,
                element: String::new(),
                reason: format!("{} 缺少 expect", a.kind),
            });
            continue;
        }
        if !LOCATOR_ASSERTIONS.contains(&a.kind.as_str()) {
            continue;
        }
        let Some(r) = a.locator.as_ref() else {
            issues.push(GateIssue {
                location: at,
                element: String::new(),
                reason: format!("{} 缺少 locator", a.kind),
            });
            continue;
        };
        let Some(id) = r.id.as_deref() else {
            issues.push(GateIssue {
                location: at,
                element: String::new(),
                reason: "存在性断言只接受注册表元素引用（不接受内联定位器）".to_string(),
            });
            continue;
        };
        match reg.get(id) {
            None => issues.push(GateIssue {
                location: at,
                element: id.to_string(),
                reason: "不在 L2 元素注册表中（定位器幻觉）".to_string(),
            }),
            Some(e) => {
                if e.locators.is_empty() {
                    issues.push(GateIssue {
                        location: at.clone(),
                        element: id.to_string(),
                        reason: "该元素没有任何定位器".to_string(),
                    });
                } else if !e.locators.iter().any(|l| l.is_semantic()) {
                    // 关键拦截：只有位置/序号定位器的元素，问不出「在不在」。
                    // 放它过去就是给自己发一张假 PASS。
                    issues.push(GateIssue {
                        location: at,
                        element: id.to_string(),
                        reason:
                            "该元素只有位置 / 序号定位器，存在性断言会假阳性（需先补 resource_id 或 content-desc）"
                                .to_string(),
                    });
                }
            }
        }
    }

    check_gate_steps(&case.teardown, "teardown ", reg, &mut issues);
    issues
}

/// 闸门对「动作」部分的校验
fn check_gate_steps(steps: &[Step], prefix: &str, reg: &Registry, issues: &mut Vec<GateIssue>) {
    for (i, s) in steps.iter().enumerate() {
        let at = format!("{prefix}{}", s.n.unwrap_or(i as u32 + 1));
        let action = s.action.as_str();
        if !KNOWN_ACTIONS.contains(&action) {
            issues.push(GateIssue {
                location: at,
                element: String::new(),
                reason: format!("未知动作 {action:?}"),
            });
            continue;
        }
        if action == "launch" && s.component.as_deref().unwrap_or("").is_empty() {
            issues.push(GateIssue {
                location: at.clone(),
                element: String::new(),
                reason: "launch 缺少 component".to_string(),
            });
        }
        if action == "key" && s.code.is_none() && s.value.is_none() {
            issues.push(GateIssue {
                location: at.clone(),
                element: String::new(),
                reason: "key 缺少 code / value".to_string(),
            });
        }
        if !LOCATOR_ACTIONS.contains(&action) {
            continue;
        }
        let Some(r) = s.locator.as_ref() else {
            issues.push(GateIssue {
                location: at,
                element: String::new(),
                reason: format!("{action} 缺少 locator"),
            });
            continue;
        };
        if let Some(id) = r.id.as_deref() {
            match reg.get(id) {
                None => issues.push(GateIssue {
                    location: at,
                    element: id.to_string(),
                    reason: "不在 L2 元素注册表中（定位器幻觉）".to_string(),
                }),
                Some(e) if e.locators.is_empty() => issues.push(GateIssue {
                    location: at.clone(),
                    element: id.to_string(),
                    reason: "该元素没有任何定位器".to_string(),
                }),
                Some(_) => {}
            }
        } else if r.kind.is_none() || r.value.is_none() {
            issues.push(GateIssue {
                location: at,
                element: String::new(),
                reason: "内联定位器缺少 type / value".to_string(),
            });
        }
    }
}

// ==================================================================== 定位

/// 定位结果：真实控件，或位置兜底算出来的一个点
enum Hit<'a> {
    Node(&'a UiNode),
    Point(i32, i32),
}

impl Hit<'_> {
    fn center(&self) -> (i32, i32) {
        match self {
            Hit::Node(n) => (n.bounds.cx(), n.bounds.cy()),
            Hit::Point(x, y) => (*x, *y),
        }
    }
}

/// 按正则做**行首锚定**匹配，与生成侧（Python `re.match`）语义保持一致
fn re_match(pat: &str, hay: &str) -> bool {
    match regex_lite::Regex::new(pat) {
        Ok(r) => r.find(hay).is_some_and(|m| m.start() == 0),
        Err(_) => false,
    }
}

/// 沿元素的分级定位器链逐级降级定位。
///
/// `allow_positional` / `allow_index` 在做**存在性断言时必须双双关掉**（见模块头注释）。
fn locate<'a>(
    elem: &'a ElementDef,
    hier: &'a Hierarchy,
    allow_positional: bool,
    allow_index: bool,
) -> Option<(Hit<'a>, &'a LocatorDef)> {
    let nodes = hier.nodes();
    if nodes.is_empty() {
        return None;
    }
    let screen = hier.screen();

    for loc in &elem.locators {
        let v = loc.value.as_str();
        match loc.kind.as_str() {
            "resource_id" => {
                if v.is_empty() {
                    continue;
                }
                for &n in &nodes {
                    if n.resource_id == v {
                        return Some((Hit::Node(n), loc));
                    }
                }
            }
            "content_desc_regex" => {
                for &n in &nodes {
                    if !n.content_desc.is_empty() && re_match(v, &n.content_desc) {
                        return Some((Hit::Node(n), loc));
                    }
                }
            }
            "text_regex" => {
                for &n in &nodes {
                    if !n.text.is_empty() && re_match(v, &n.text) {
                        return Some((Hit::Node(n), loc));
                    }
                }
            }
            "content_desc" => {
                if v.is_empty() {
                    continue;
                }
                for &n in &nodes {
                    if n.content_desc == v {
                        return Some((Hit::Node(n), loc));
                    }
                }
            }
            "text" => {
                if v.is_empty() {
                    continue;
                }
                for &n in &nodes {
                    if n.text == v {
                        return Some((Hit::Node(n), loc));
                    }
                }
            }
            "bounds_center" => {
                if !allow_positional || screen.w() <= 0 || screen.h() <= 0 {
                    continue;
                }
                let Some((rx, ry)) = parse_pair(v) else {
                    continue;
                };
                let x = (rx * screen.w() as f32).round() as i32;
                let y = (ry * screen.h() as f32).round() as i32;
                if (0..=screen.w()).contains(&x) && (0..=screen.h()).contains(&y) {
                    return Some((Hit::Point(x, y), loc));
                }
            }
            "class_index" => {
                if !allow_index {
                    continue;
                }
                let Some((cls, idx)) = v.split_once('#') else {
                    continue;
                };
                let idx: usize = match idx.trim().parse() {
                    Ok(i) => i,
                    Err(_) => continue,
                };
                // 该 class 的第 idx 个节点（深度优先序）
                if let Some(&n) = nodes.iter().filter(|n| n.short_class() == cls).nth(idx) {
                    return Some((Hit::Node(n), loc));
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_pair(v: &str) -> Option<(f32, f32)> {
    let (a, b) = v.split_once(',')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

// ==================================================================== 断言

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AssertStatus {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssertResult {
    pub n: u32,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
    /// 用例作者写的这条断言在验什么（需求追溯的可读补充）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub status: AssertStatus,
    pub msg: String,
    pub attempts: u32,
}

#[derive(Debug, Clone)]
pub struct AttemptOutcome {
    pub pass: bool,
    pub msg: String,
    /// 该断言是否根本没被评估（例如缺 --package 时无法监控崩溃）
    pub skipped: bool,
}

impl AttemptOutcome {
    fn pass(msg: impl Into<String>) -> Self {
        Self {
            pass: true,
            msg: msg.into(),
            skipped: false,
        }
    }
    fn fail(msg: impl Into<String>) -> Self {
        Self {
            pass: false,
            msg: msg.into(),
            skipped: false,
        }
    }
    fn skip(msg: impl Into<String>) -> Self {
        Self {
            pass: false,
            msg: msg.into(),
            skipped: true,
        }
    }
}

fn dump_page(dev: &mut dyn Device) -> Result<Hierarchy> {
    let xml = dev.dump_xml()?;
    dump::parse_hierarchy(&xml)
}

/// 在一个页面上评估断言，返回 `(是否满足, 说明)`
fn eval_on_page(a: &Assertion, reg: &Registry, hier: &Hierarchy) -> (bool, String) {
    let expect = a.expect.clone().unwrap_or_default();
    match a.kind.as_str() {
        "text_visible" | "text_contains" => {
            for n in hier.nodes() {
                let hay = if !n.text.is_empty() {
                    n.text.as_str()
                } else {
                    n.content_desc.as_str()
                };
                if !hay.is_empty() && hay.contains(expect.as_str()) {
                    return (true, format!("命中 '{}'", util::truncate(hay, 40)));
                }
            }
            (
                false,
                format!(
                    "未找到文本 '{}'（已 dump {} 个节点）",
                    expect,
                    hier.node_count()
                ),
            )
        }
        "text_equals" => {
            for n in hier.nodes() {
                if n.text == expect || n.content_desc == expect {
                    return (true, format!("命中 '{}'", util::truncate(&expect, 40)));
                }
            }
            (
                false,
                format!("未找到文本 '{}'（精确匹配）", util::truncate(&expect, 40)),
            )
        }
        "element_exists" | "element_absent" => {
            let want_present = a.kind == "element_exists";
            let Some(r) = a.locator.as_ref() else {
                return (false, "断言缺少 locator".to_string());
            };
            let Some(elem) = r.id.as_deref().and_then(|id| reg.get(id)) else {
                return (false, "断言引用的元素不在注册表".to_string());
            };
            // ★ 存在性断言：只认语义定位器
            let found = locate(elem, hier, false, false);
            match (want_present, found) {
                (true, Some((_, loc))) => (true, format!("找到 {}（via {}）", elem.id, loc.kind)),
                (false, None) => (true, format!("元素 {} 确实不存在", elem.id)),
                (true, None) => (false, format!("元素 {} 未出现在当前页面", elem.id)),
                (false, Some((_, loc))) => (
                    false,
                    format!("元素 {} 仍在，预期应消失（via {}）", elem.id, loc.kind),
                ),
            }
        }
        _ => (false, format!("执行器不处理该断言类型: {}", a.kind)),
    }
}

/// 带重试的断言：一直重试到超时。
///
/// 重试是必要的 ——「点了按钮，提示还没弹出来」是最常见的时序问题，
/// 但重试**只能等**，不能降低判据严格程度（否则又变成假阳性）。
fn check_with_retry<F>(timeout_ms: u64, verbose: bool, mut attempt: F) -> AssertResult
where
    F: FnMut() -> AttemptOutcome,
{
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut n = 0u32;
    loop {
        n += 1;
        let out = attempt();
        let status = if out.skipped {
            AssertStatus::Skip
        } else if out.pass {
            AssertStatus::Pass
        } else {
            AssertStatus::Fail
        };
        if status != AssertStatus::Fail {
            return AssertResult {
                n: 0,
                kind: String::new(),
                expect: None,
                note: None,
                status,
                msg: out.msg,
                attempts: n,
            };
        }
        // 重试是要等的（提示可能还没弹出来），但「为什么还没满足」必须看得到 ——
        // 否则一个 4 秒的超时失败只留下一句「未找到」，没法判断是慢还是真的不对。
        if verbose {
            println!("    · 第 {} 次未满足：{}", n, util::truncate(&out.msg, 140));
        }
        if Instant::now() >= deadline {
            return AssertResult {
                n: 0,
                kind: String::new(),
                expect: None,
                note: None,
                status: AssertStatus::Fail,
                msg: format!("{}（重试 {} 次）", out.msg, n),
                attempts: n,
            };
        }
        std::thread::sleep(RETRY_INTERVAL);
    }
}

/// 执行一条断言（不含 `no_crash`，它按用例粒度结算）
fn run_assertion(
    a: &Assertion,
    reg: &Registry,
    dev: &mut dyn Device,
    verbose: bool,
) -> AssertResult {
    let timeout = a.timeout_ms.unwrap_or(DEFAULT_ASSERT_TIMEOUT_MS);
    let mut r = match a.kind.as_str() {
        "activity_is" => {
            let expect = a.expect.clone().unwrap_or_default();
            check_with_retry(timeout, verbose, || match device::current_activity(&*dev) {
                Ok(act) if act.contains(expect.as_str()) => {
                    AttemptOutcome::pass(format!("当前 Activity = {act}"))
                }
                Ok(act) => AttemptOutcome::fail(format!(
                    "当前 Activity = {}，预期含 '{}'",
                    if act.is_empty() { "(空)" } else { &act },
                    expect
                )),
                Err(e) => AttemptOutcome::fail(format!("取前台 Activity 失败: {e}")),
            })
        }
        _ => check_with_retry(timeout, verbose, || match dump_page(dev) {
            Ok(hier) => {
                let (ok, msg) = eval_on_page(a, reg, &hier);
                if ok {
                    AttemptOutcome::pass(msg)
                } else {
                    AttemptOutcome::fail(msg)
                }
            }
            Err(e) => AttemptOutcome::fail(format!(
                "dump 失败（{}）—— 页面可能有持续动画，等不到 UI 空闲",
                util::truncate(&e.to_string(), 120)
            )),
        }),
    };
    r.n = a.n.unwrap_or(0);
    r.kind = a.kind.clone();
    r.expect = a.expect.clone();
    r.note = a.desc.clone();
    r
}

/// `no_crash` 的判定：只看**用例执行期间**新增的、归因到被测应用的异常。
///
/// 单独抽成纯函数是为了可测：崩溃监控依赖设备 logcat，
/// 但「拿到哪些异常才算失败」这条规则本身不该依赖设备。
pub fn no_crash_outcome(monitored: bool, during: &[Incident]) -> AttemptOutcome {
    if !monitored {
        return AttemptOutcome::skip(
            "未指定 --package，无法把崩溃日志归因到被测应用（该断言已跳过）",
        );
    }
    if during.is_empty() {
        return AttemptOutcome::pass("用例执行期间无 crash / ANR");
    }
    let kinds: Vec<String> = during
        .iter()
        .map(|i| format!("{}({})", i.kind.cn(), util::truncate(&i.summary, 60)))
        .collect();
    AttemptOutcome::fail(format!(
        "用例执行期间出现 {} 个异常：{}",
        during.len(),
        kinds.join(" / ")
    ))
}

// ==================================================================== 执行

#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
    pub n: u32,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub element: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// L2 里的控件角色（button / tab / input …）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
    /// 命中的定位器置信度：越低越依赖兜底（位置 / 序号）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<(i32, i32)>,
    /// 用例作者写的这一步在做什么
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub stage: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub id: String,
    pub title: String,
    pub priority: String,
    pub tags: Vec<String>,
    pub trace: Trace,
    pub status: String,
    pub duration_ms: u64,
    pub gate: Vec<GateIssue>,
    pub steps: Vec<StepResult>,
    pub assertions: Vec<AssertResult>,
    pub incidents: Vec<Incident>,
}

impl CaseResult {
    fn blank(case: &Case, status: &str) -> Self {
        Self {
            id: case.id.clone(),
            title: case.title.clone(),
            priority: case.priority.clone(),
            tags: case.tags.clone(),
            trace: case.trace.clone(),
            status: status.to_string(),
            duration_ms: 0,
            gate: Vec::new(),
            steps: Vec::new(),
            assertions: Vec::new(),
            incidents: Vec::new(),
        }
    }
}

fn action_of(step: &Step, reg: &Registry, hier: &Hierarchy) -> Result<(Action, StepResult)> {
    let mut sr = StepResult {
        n: step.n.unwrap_or(0),
        action: step.action.clone(),
        element: None,
        label: None,
        role: None,
        locator: None,
        confidence: None,
        point: None,
        note: step.desc.clone(),
        stage: "step".to_string(),
        ok: true,
        detail: String::new(),
    };

    match step.action.as_str() {
        "back" => {
            sr.detail = "keyevent 4 (back)".to_string();
            Ok((Action::Back, sr))
        }
        "launch" => {
            let component = step.component.clone().unwrap_or_default();
            sr.detail = component.clone();
            Ok((Action::Launch { component }, sr))
        }
        "key" => {
            let code = step.code.or_else(|| {
                let v = step.value.clone().unwrap_or_default();
                v.trim().parse::<i32>().ok().or_else(|| match v.trim() {
                    "back" => Some(4),
                    "home" => Some(3),
                    "enter" => Some(66),
                    "menu" => Some(82),
                    _ => None,
                })
            });
            let Some(code) = code else {
                bail!("key 动作缺少可用按键（code 或 value）");
            };
            sr.detail = format!("keyevent {code}");
            Ok((
                Action::Key {
                    code,
                    name: String::new(),
                },
                sr,
            ))
        }
        "swipe" => {
            let dir = match step.direction.as_deref().unwrap_or("up") {
                "up" => SwipeDir::Up,
                "down" => SwipeDir::Down,
                "left" => SwipeDir::Left,
                "right" => SwipeDir::Right,
                other => bail!("未知滑动方向 {other:?}"),
            };
            let elem = step
                .locator
                .as_ref()
                .and_then(|r| r.id.as_deref())
                .and_then(|id| reg.get(id));
            let area = match elem {
                Some(e) => match locate(e, hier, true, true) {
                    Some((Hit::Node(n), loc)) => {
                        sr.element = Some(e.id.clone());
                        sr.label = Some(e.label());
                        sr.role = (!e.role.is_empty()).then(|| e.role.clone());
                        sr.locator = Some(loc.kind.clone());
                        sr.confidence = Some(loc.confidence);
                        n.bounds
                    }
                    _ => bail!("swipe 目标 {} 不在当前页面", e.id),
                },
                None => hier.screen(),
            };
            let (from, to) = swipe_points(area, dir);
            sr.point = Some(from);
            sr.detail = format!("{} {:?}->{:?}", dir.cn(), from, to);
            Ok((
                Action::Swipe {
                    target: None,
                    from,
                    to,
                    dir,
                    duration_ms: SWIPE_MS,
                },
                sr,
            ))
        }
        action @ ("click" | "long_click" | "input") => {
            let Some(r) = step.locator.as_ref() else {
                bail!("{action} 缺少 locator");
            };
            let Some(id) = r.id.as_deref() else {
                bail!("{action} 只接受注册表元素引用");
            };
            let Some(e) = reg.get(id) else {
                bail!("元素 {id} 不在注册表");
            };
            let Some((hit, loc)) = locate(e, hier, true, true) else {
                bail!("元素 {id} 不在当前页面");
            };
            let (x, y) = hit.center();
            let target = match hit {
                Hit::Node(n) => TargetInfo::from_node(n),
                Hit::Point(..) => TargetInfo {
                    key: e.id.clone(),
                    label: e.label(),
                    ..TargetInfo::default()
                },
            };
            sr.element = Some(e.id.clone());
            sr.label = Some(e.label());
            sr.role = (!e.role.is_empty()).then(|| e.role.clone());
            sr.locator = Some(loc.kind.clone());
            sr.confidence = Some(loc.confidence);
            sr.point = Some((x, y));
            sr.detail = format!("{} @({},{}) [{}]", e.label(), x, y, loc.kind);

            let act = match action {
                "click" => Action::Click {
                    target,
                    point: (x, y),
                },
                "long_click" => Action::LongClick {
                    target,
                    point: (x, y),
                },
                _ => Action::Input {
                    target,
                    point: (x, y),
                    text: step.value.clone().unwrap_or_default(),
                },
            };
            Ok((act, sr))
        }
        other => bail!("未知动作 {other:?}"),
    }
}

/// 在给定区域内按方向取一条滑动轨迹
fn swipe_points(area: crate::model::Rect, dir: SwipeDir) -> ((i32, i32), (i32, i32)) {
    let (cx, cy) = (area.cx(), area.cy());
    let (w, h) = (area.w(), area.h());
    match dir {
        SwipeDir::Up => ((cx, area.y1 + h * 3 / 4), (cx, area.y1 + h / 4)),
        SwipeDir::Down => ((cx, area.y1 + h / 4), (cx, area.y1 + h * 3 / 4)),
        SwipeDir::Left => ((area.x1 + w * 3 / 4, cy), (area.x1 + w / 4, cy)),
        SwipeDir::Right => ((area.x1 + w / 4, cy), (area.x1 + w * 3 / 4, cy)),
    }
}

fn run_steps(
    steps: &[Step],
    stage: &str,
    reg: &Registry,
    dev: &mut dyn Device,
    settle_ms: u64,
    out: &mut Vec<StepResult>,
) -> bool {
    for (i, s) in steps.iter().enumerate() {
        let n = s.n.unwrap_or(i as u32 + 1);
        if s.action == "wait" {
            let ms = s
                .value
                .as_deref()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(settle_ms);
            std::thread::sleep(Duration::from_millis(ms));
            out.push(StepResult {
                n,
                action: "wait".to_string(),
                element: None,
                label: None,
                role: None,
                locator: None,
                confidence: None,
                point: None,
                note: s.desc.clone(),
                stage: stage.to_string(),
                ok: true,
                detail: format!("等待 {ms}ms"),
            });
            println!("  {stage:<8} {n:<3} wait     等待 {ms}ms");
            continue;
        }

        let hier = match dump_page(dev) {
            Ok(h) => h,
            Err(e) => {
                out.push(StepResult {
                    n,
                    action: s.action.clone(),
                    element: s.locator.as_ref().and_then(|r| r.id.clone()),
                    label: None,
                    role: None,
                    locator: None,
                    confidence: None,
                    point: None,
                    note: s.desc.clone(),
                    stage: stage.to_string(),
                    ok: false,
                    detail: format!("dump 失败: {e}"),
                });
                println!("  {stage:<8} {n:<3} FAIL     dump 失败，无法定位：{e}");
                return false;
            }
        };

        let built = match action_of(s, reg, &hier) {
            Ok(v) => v,
            Err(e) => {
                out.push(StepResult {
                    n,
                    action: s.action.clone(),
                    element: s.locator.as_ref().and_then(|r| r.id.clone()),
                    label: None,
                    role: None,
                    locator: None,
                    confidence: None,
                    point: None,
                    note: s.desc.clone(),
                    stage: stage.to_string(),
                    ok: false,
                    detail: e.to_string(),
                });
                println!("  {stage:<8} {n:<3} FAIL     {e}");
                return false;
            }
        };
        let (action, mut sr) = built;
        sr.stage = stage.to_string();

        match runner::execute_action(&*dev, &action) {
            Ok(()) => {
                if sr.element.is_some() {
                    let (px, py) = sr.point.unwrap_or((0, 0));
                    println!(
                        "  {stage:<8} {n:<3} {:<9}{:<18} ({}) @{},{} [{}]",
                        s.action,
                        sr.label.clone().unwrap_or_default(),
                        sr.element.clone().unwrap_or_default(),
                        px,
                        py,
                        sr.locator.clone().unwrap_or_default()
                    );
                } else {
                    // back / key / launch / wait 这类没有定位目标的动作，打印它实际下发的指令
                    println!("  {stage:<8} {n:<3} {:<9}{}", s.action, sr.detail);
                }
                out.push(sr);
                if settle_ms > 0 {
                    std::thread::sleep(Duration::from_millis(settle_ms));
                }
            }
            Err(e) => {
                sr.ok = false;
                sr.detail = format!("动作下发失败: {e}");
                println!("  {stage:<8} {n:<3} FAIL     {e}");
                out.push(sr);
                return false;
            }
        }
    }
    true
}

// ==================================================================== 一次完整执行

pub struct ScriptOptions {
    pub kb: PathBuf,
    pub target: PathBuf,
    pub out: Option<PathBuf>,
    /// 被测包名：用于把崩溃日志归因到应用（不填则 `no_crash` 跳过）
    pub package: Option<String>,
    pub settle_ms: u64,
    pub filter: Option<String>,
    pub dry_run: bool,
    pub verbose: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Summary {
    pub total: usize,
    pub pass: usize,
    pub fail: usize,
    pub blocked: usize,
    /// 断言全部被跳过的用例数（既不算通过也不算失败）
    pub skip: usize,
    pub skipped_assertions: usize,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScriptRun {
    pub generated: String,
    pub kb: String,
    pub package: String,
    pub mode: String,
    pub summary: Summary,
    pub cases: Vec<CaseResult>,
}

impl ScriptRun {
    pub fn has_problem(&self) -> bool {
        self.summary.fail > 0 || self.summary.blocked > 0
    }
}

/// 载入用例：目录则取 `*.yaml`（按文件名排序），否则当单个文件
fn collect_cases(target: &Path) -> Result<Vec<(PathBuf, Result<Case>)>> {
    let paths: Vec<PathBuf> = if target.is_dir() {
        let mut v: Vec<PathBuf> = std::fs::read_dir(target)
            .with_context(|| format!("无法读取 {}", target.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "yaml" || x == "yml"))
            .collect();
        v.sort();
        v
    } else {
        vec![target.to_path_buf()]
    };
    if paths.is_empty() {
        bail!("{} 下没有 .yaml 用例", target.display());
    }
    Ok(paths
        .into_iter()
        .map(|p| {
            let r = std::fs::read_to_string(&p)
                .map_err(anyhow::Error::from)
                .and_then(|t| serde_yaml::from_str::<Case>(&t).map_err(anyhow::Error::from))
                .with_context(|| format!("解析用例 {} 失败", p.display()));
            (p, r)
        })
        .collect())
}

pub fn run(opts: &ScriptOptions, mut dev: Option<&mut dyn Device>) -> Result<ScriptRun> {
    let reg = Registry::load(&opts.kb)?;
    let cases = collect_cases(&opts.target)?;
    let t_all = Instant::now();

    if !opts.dry_run && dev.is_none() {
        bail!("非 dry-run 模式需要设备（内部错误：未传入设备）");
    }
    // 设备的运行形态（device / host / mock），报告里要如实标注 —— 别把模拟执行说成真机执行
    let dev_kind = dev.as_deref().map(|d| d.kind()).unwrap_or("none");
    if opts.dry_run {
        println!(
            "[闸门] 只做定位器校验，不连设备。注册表 {}（{} 个元素文件 / {} 个元素）",
            opts.kb.display(),
            reg.files(),
            reg.len()
        );
    } else {
        println!(
            "[执行] 元素注册表 {}（{} 个文件 / {} 个元素），用例 {} 条",
            opts.kb.display(),
            reg.files(),
            reg.len(),
            cases.len()
        );
    }

    // ---- 崩溃监控：整个套件开一次，按用例结算
    // 目录名时间戳只到秒，同一秒内连跑两次会撞车 → 用 unique_dir 确保不覆盖
    let out_dir = opts.out.clone().unwrap_or_else(|| {
        util::unique_dir(
            Path::new("reports"),
            &format!("script_{}", util::now_file_str()),
        )
    });
    util::ensure_dir(&out_dir)?;

    let mut monitor: Option<LogcatMonitor> = None;
    if !opts.dry_run {
        if let (Some(pkg), Some(d)) = (opts.package.as_deref(), dev.as_deref()) {
            match LogcatMonitor::start(d, pkg, &out_dir.join("logcat.txt")) {
                Ok(m) => monitor = Some(m),
                Err(e) => println!("[警告] 崩溃监控启动失败，no_crash 断言将跳过：{e}"),
            }
        }
    }

    let mut summary = Summary {
        total: cases.len(),
        ..Default::default()
    };
    let mut results: Vec<CaseResult> = Vec::new();

    for (path, parsed) in &cases {
        let case = match parsed {
            Ok(c) => c,
            Err(e) => {
                let mut r = CaseResult::blank(
                    &Case {
                        id: path
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        ..Default::default()
                    },
                    "blocked",
                );
                r.gate.push(GateIssue {
                    location: "load".to_string(),
                    element: String::new(),
                    reason: format!("{e:#}"),
                });
                println!("用例 {}  解析失败，已阻塞：{e}", r.id);
                summary.blocked += 1;
                results.push(r);
                continue;
            }
        };

        if let Some(f) = opts.filter.as_deref() {
            if !case.id.contains(f) && !case.title.contains(f) {
                continue;
            }
        }

        // ---- ★ 闸门
        let gate = gate_case(case, &reg);
        if !gate.is_empty() {
            let mut r = CaseResult::blank(case, "blocked");
            println!("用例 {}  {}", case.id, case.title);
            for g in &gate {
                let who = if g.element.is_empty() {
                    String::new()
                } else {
                    format!("元素 {}：", g.element)
                };
                println!("  闸门 BLOCKED  {:<9} {}{}", g.location, who, g.reason);
            }
            println!("  => BLOCKED（未执行）\n");
            r.gate = gate;
            summary.blocked += 1;
            results.push(r);
            continue;
        }

        if opts.dry_run {
            let mut r = CaseResult::blank(case, "verified");
            let steps = case.setup.len() + case.steps.len() + case.teardown.len();
            println!(
                "用例 {}  {}\n  闸门 VERIFIED   {} 个动作 / {} 条断言，元素引用全部命中 L2\n  => VERIFIED\n",
                case.id,
                case.title,
                steps,
                case.assertions.len()
            );
            r.gate.clear();
            summary.pass += 1;
            results.push(r);
            continue;
        }

        let d = dev
            .as_deref_mut()
            .ok_or_else(|| anyhow::anyhow!("内部错误：缺少设备"))?;
        let r = run_case(case, &reg, d, opts, monitor.as_ref());
        summary.skipped_assertions += r
            .assertions
            .iter()
            .filter(|a| a.status == AssertStatus::Skip)
            .count();
        match r.status.as_str() {
            "pass" => summary.pass += 1,
            "skip" => summary.skip += 1,
            "fail" => summary.fail += 1,
            _ => summary.blocked += 1,
        }
        if !r.incidents.is_empty() {
            println!("  ⚠ 期间捕获 {} 个 crash/ANR", r.incidents.len());
        }
        results.push(r);
    }

    if let Some(m) = &monitor {
        for inc in m.poll() {
            println!(
                "[异常] {} {} {}",
                inc.kind.cn(),
                inc.package,
                util::truncate(&inc.summary, 80)
            );
        }
        m.stop();
    }

    summary.duration_ms = util::elapsed_ms(t_all);

    let run = ScriptRun {
        generated: util::now_str(),
        kb: opts.kb.display().to_string(),
        package: opts.package.clone().unwrap_or_default(),
        mode: if opts.dry_run {
            "dry-run".to_string()
        } else if dev_kind == "mock" {
            "mock".to_string()
        } else {
            format!("execute:{dev_kind}")
        },
        summary,
        cases: results,
    };

    // 报告两种模式都写：闸门报告本身就是给人看的产物（哪些用例被拦下、为什么）
    let json = serde_json::to_string_pretty(&run)?;
    std::fs::write(out_dir.join("cases.json"), json)?;
    std::fs::write(out_dir.join("report.html"), render_html(&run))?;
    println!("[报告] {}", out_dir.join("report.html").display());
    println!("[数据] {}", out_dir.join("cases.json").display());

    print_summary(&run);
    Ok(run)
}

fn run_case(
    case: &Case,
    reg: &Registry,
    dev: &mut dyn Device,
    opts: &ScriptOptions,
    monitor: Option<&LogcatMonitor>,
) -> CaseResult {
    let t0 = Instant::now();
    let mut r = CaseResult::blank(case, "pass");
    println!("用例 {}  {}", case.id, case.title);

    // 用例起点：先排空累积的异常，后面的都算这条用例的
    if let Some(m) = monitor {
        let _ = m.poll();
    }

    let mut ok = true;
    if !case.setup.is_empty() {
        ok = run_steps(&case.setup, "setup", reg, dev, opts.settle_ms, &mut r.steps);
    }
    if ok {
        ok = run_steps(&case.steps, "step", reg, dev, opts.settle_ms, &mut r.steps);
    }
    if !ok {
        r.status = "fail".to_string();
    }

    // ---- 断言（no_crash 放到最后按用例结算）
    if ok {
        for (i, a) in case.assertions.iter().enumerate() {
            if a.kind == "no_crash" {
                continue;
            }
            let mut res = run_assertion(a, reg, dev, opts.verbose);
            if res.n == 0 {
                res.n = a.n.unwrap_or(i as u32 + 1);
            }
            print_assert(&res);
            if res.status == AssertStatus::Fail {
                r.status = "fail".to_string();
            }
            r.assertions.push(res);
        }
    }

    // ---- 崩溃结算：只看这条用例执行期间新增的、归因到被测应用的异常
    let during = monitor.map(|m| m.poll()).unwrap_or_default();
    for (i, a) in case.assertions.iter().enumerate() {
        if a.kind != "no_crash" {
            continue;
        }
        let out = no_crash_outcome(monitor.is_some(), &during);
        let status = if out.skipped {
            AssertStatus::Skip
        } else if out.pass {
            AssertStatus::Pass
        } else {
            AssertStatus::Fail
        };
        let res = AssertResult {
            n: a.n.unwrap_or(i as u32 + 1),
            kind: a.kind.clone(),
            expect: None,
            note: a.desc.clone(),
            status,
            msg: out.msg,
            attempts: 1,
        };
        print_assert(&res);
        if status == AssertStatus::Fail {
            r.status = "fail".to_string();
        }
        r.assertions.push(res);
    }
    r.incidents = during;

    // ---- 收尾（失败也执行，尽量把环境还原）
    if !case.teardown.is_empty() {
        let before = r.status.clone();
        run_steps(
            &case.teardown,
            "teardown",
            reg,
            dev,
            opts.settle_ms,
            &mut r.steps,
        );
        r.status = before;
    }

    // 全部断言都被跳过 ⇒ 这条用例其实什么都没验证。
    // 报成 PASS 就是给自己发假 PASS —— 「没测到的用例看起来是绿的」比 FAIL 危险得多。
    if r.status == "pass"
        && !r.assertions.is_empty()
        && r.assertions.iter().all(|a| a.status == AssertStatus::Skip)
    {
        r.status = "skip".to_string();
    }

    r.duration_ms = util::elapsed_ms(t0);
    println!(
        "  => {}（{}）\n",
        r.status.to_uppercase(),
        util::fmt_duration(r.duration_ms)
    );
    r
}

fn print_assert(res: &AssertResult) {
    let tag = match res.status {
        AssertStatus::Pass => "PASS",
        AssertStatus::Fail => "FAIL",
        AssertStatus::Skip => "SKIP",
    };
    let mark = if res.status == AssertStatus::Fail {
        "  <--"
    } else {
        ""
    };
    println!(
        "  assert {:<3} {:<4}  {:<22} {}{}",
        res.n, tag, res.kind, res.msg, mark
    );
}

fn print_summary(run: &ScriptRun) {
    let s = &run.summary;
    println!("{}", "=".repeat(52));
    if run.mode == "dry-run" {
        println!(
            "闸门通过 {} / 阻塞 {}（共 {} 条）—— 未连设备",
            s.pass, s.blocked, s.total
        );
    } else if s.skip > 0 {
        println!(
            "通过 {} / 失败 {} / 阻塞 {} / 跳过 {}（共 {} 条，{}）",
            s.pass,
            s.fail,
            s.blocked,
            s.skip,
            s.total,
            util::fmt_duration(s.duration_ms)
        );
    } else {
        println!(
            "通过 {} / 失败 {} / 阻塞 {}（共 {} 条，{}）",
            s.pass,
            s.fail,
            s.blocked,
            s.total,
            util::fmt_duration(s.duration_ms)
        );
    }
    if s.skipped_assertions > 0 {
        println!(
            "注意：有 {} 条断言被跳过（未评估），对应用例不计入「通过」",
            s.skipped_assertions
        );
    }
}

// ==================================================================== 报告

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn status_pill(status: &str) -> String {
    let (cls, text) = match status {
        "pass" => ("ok", "PASS".to_string()),
        "fail" => ("bad", "FAIL".to_string()),
        "blocked" => ("warn", "BLOCKED".to_string()),
        "skip" => ("muted", "SKIP".to_string()),
        "verified" => ("ok", "VERIFIED".to_string()),
        _ => ("muted", status.to_uppercase()),
    };
    format!("<span class=\"pill {cls}\">{text}</span>")
}

fn render_html(run: &ScriptRun) -> String {
    let s = &run.summary;
    let mut h = String::with_capacity(16 * 1024);
    h.push_str(
        "<!DOCTYPE html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
<title>用例执行报告</title><style>\
:root{--bg:#0f1116;--card:#171a21;--line:#262b36;--fg:#e6e9ef;--dim:#98a2b3;\
--ok:#3fb950;--bad:#f85149;--warn:#d29922;--acc:#58a6ff}\
*{box-sizing:border-box}\
body{margin:0;padding:28px;background:var(--bg);color:var(--fg);\
font:14px/1.6 -apple-system,'Segoe UI','Microsoft YaHei',sans-serif}\
h1{font-size:20px;margin:0 0 4px}\
.sub{color:var(--dim);font-size:12px;margin-bottom:18px}\
.cards{display:flex;gap:10px;flex-wrap:wrap;margin-bottom:22px}\
.card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:12px 16px;min-width:96px}\
.card b{display:block;font-size:22px;line-height:1.2}\
.card span{color:var(--dim);font-size:12px}\
.case{background:var(--card);border:1px solid var(--line);border-radius:10px;\
padding:14px 16px;margin-bottom:14px}\
.case h2{font-size:15px;margin:0 0 2px}\
.case .meta{color:var(--dim);font-size:12px;margin-bottom:10px}\
.pill{display:inline-block;padding:1px 9px;border-radius:20px;font-size:11px;font-weight:600;\
border:1px solid currentColor}\
.pill.ok{color:var(--ok)}.pill.bad{color:var(--bad)}.pill.warn{color:var(--warn)}.pill.muted{color:var(--dim)}\
table{width:100%;border-collapse:collapse;margin:6px 0 10px;font-size:13px}\
th,td{text-align:left;padding:5px 8px;border-bottom:1px solid var(--line);vertical-align:top}\
th{color:var(--dim);font-weight:500;font-size:12px}\
td.mono,th.mono{font-family:ui-monospace,Consolas,monospace;font-size:12px}\
.dim{color:var(--dim);font-size:11px}\
.ok-t{color:var(--ok)}.bad-t{color:var(--bad)}.warn-t{color:var(--warn)}\
.foot{color:var(--dim);font-size:12px;margin-top:22px}\
</style></head><body>",
    );
    h.push_str(&format!(
        "<h1>用例执行报告</h1><div class=\"sub\">{} · {} · 知识库 {} · 包名 {}</div>",
        esc(&run.generated),
        match run.mode.as_str() {
            "dry-run" => "定位器校验（未连设备）",
            "mock" => "模拟设备执行（无真机）",
            "execute:host" => "真机执行（PC 端 adb 驱动）",
            "execute:device" => "真机执行（端上执行）",
            _ => "真机执行",
        },
        esc(&run.kb),
        if run.package.is_empty() {
            "(未指定)"
        } else {
            &run.package
        }
    ));

    h.push_str("<div class=\"cards\">");
    let mut cards: Vec<(usize, &str)> = vec![
        (s.total, "用例总数"),
        (s.pass, "通过"),
        (s.fail, "失败"),
        (s.blocked, "阻塞"),
    ];
    if s.skip > 0 {
        cards.push((s.skip, "跳过"));
    }
    for (v, k) in cards {
        h.push_str(&format!(
            "<div class=\"card\"><b>{v}</b><span>{k}</span></div>"
        ));
    }
    h.push_str(&format!(
        "<div class=\"card\"><b>{}</b><span>耗时</span></div>",
        util::fmt_duration(s.duration_ms)
    ));
    h.push_str("</div>");

    for c in &run.cases {
        h.push_str("<div class=\"case\">");
        h.push_str(&format!(
            "<h2>{} {}</h2>",
            status_pill(&c.status),
            esc(&c.id)
        ));
        let mut meta: Vec<String> = Vec::new();
        if !c.title.is_empty() {
            meta.push(esc(&c.title));
        }
        if !c.priority.is_empty() {
            meta.push(format!("优先级 {}", esc(&c.priority)));
        }
        if !c.tags.is_empty() {
            meta.push(esc(&c.tags.join(", ")));
        }
        if !c.trace.requirement.is_empty() {
            meta.push(format!("需求 {}", esc(&c.trace.requirement)));
        }
        if !c.trace.dimension.is_empty() {
            meta.push(esc(&c.trace.dimension));
        }
        if c.duration_ms > 0 {
            meta.push(util::fmt_duration(c.duration_ms));
        }
        h.push_str(&format!("<div class=\"meta\">{}</div>", meta.join(" · ")));

        if !c.gate.is_empty() {
            h.push_str("<table><tr><th>位置</th><th>元素</th><th>阻断原因</th></tr>");
            for g in &c.gate {
                h.push_str(&format!(
                    "<tr><td class=\"mono warn-t\">{}</td><td class=\"mono\">{}</td><td>{}</td></tr>",
                    esc(&g.location),
                    esc(&g.element),
                    esc(&g.reason)
                ));
            }
            h.push_str("</table>");
        }

        if !c.steps.is_empty() {
            h.push_str(
                "<table><tr><th>#</th><th>阶段</th><th>动作 / 说明</th><th>元素</th>\
<th>定位器</th><th>坐标</th><th>结果</th></tr>",
            );
            for st in &c.steps {
                let cls = if st.ok { "ok-t" } else { "bad-t" };
                let note = match st.note.as_deref() {
                    Some(t) if !t.is_empty() => {
                        format!("<br><span class=\"dim\">{}</span>", esc(t))
                    }
                    _ => String::new(),
                };
                let role = match st.role.as_deref() {
                    Some(r) if !r.is_empty() => format!(" <span class=\"dim\">[{r}]</span>"),
                    _ => String::new(),
                };
                let loc = match (st.locator.as_deref(), st.confidence) {
                    (Some(k), Some(cf)) => format!("{k}({cf:.2})"),
                    (Some(k), None) => k.to_string(),
                    _ => String::new(),
                };
                let elem = format!("{}{}", esc(st.element.as_deref().unwrap_or("")), role);
                let point = st
                    .point
                    .map(|(x, y)| format!("{x},{y}"))
                    .unwrap_or_default();
                h.push_str(&format!(
                    "<tr><td class=\"mono\">{}</td><td>{}</td><td>{}{}</td><td>{}<br>\
<span class=\"dim mono\">{}</span></td><td class=\"mono\">{}</td>\
<td class=\"mono\">{}</td><td class=\"{}\">{}</td></tr>",
                    st.n,
                    esc(&st.stage),
                    esc(&st.action),
                    note,
                    esc(st.label.as_deref().unwrap_or("")),
                    elem,
                    esc(&loc),
                    point,
                    cls,
                    if st.ok {
                        "OK".to_string()
                    } else {
                        esc(&st.detail)
                    }
                ));
            }
            h.push_str("</table>");
        }

        if !c.assertions.is_empty() {
            h.push_str(
                "<table><tr><th>#</th><th>断言</th><th>期望 / 说明</th><th>结果</th><th>说明</th></tr>",
            );
            for a in &c.assertions {
                let (cls, tag) = match a.status {
                    AssertStatus::Pass => ("ok-t", "PASS"),
                    AssertStatus::Fail => ("bad-t", "FAIL"),
                    AssertStatus::Skip => ("warn-t", "SKIP"),
                };
                let note = match a.note.as_deref() {
                    Some(t) if !t.is_empty() => {
                        format!("<br><span class=\"dim\">{}</span>", esc(t))
                    }
                    _ => String::new(),
                };
                h.push_str(&format!(
                    "<tr><td class=\"mono\">{}</td><td class=\"mono\">{}</td><td class=\"mono\">{}{}</td>\
<td class=\"{}\">{}</td><td>{}</td></tr>",
                    a.n,
                    esc(&a.kind),
                    esc(a.expect.as_deref().unwrap_or("")),
                    note,
                    cls,
                    tag,
                    esc(&a.msg)
                ));
            }
            h.push_str("</table>");
        }

        if !c.incidents.is_empty() {
            h.push_str("<table><tr><th>类型</th><th>包名</th><th>摘要</th></tr>");
            for i in &c.incidents {
                h.push_str(&format!(
                    "<tr><td class=\"bad-t\">{}</td><td class=\"mono\">{}</td><td>{}</td></tr>",
                    esc(i.kind.cn()),
                    esc(&i.package),
                    esc(&i.summary)
                ));
            }
            h.push_str("</table>");
        }

        h.push_str("</div>");
    }

    h.push_str(
        "<div class=\"foot\">存在性断言只使用语义定位器（resource_id / content_desc / text / 正则）；\
位置与序号定位器不参与「控件在不在」的判断。</div>",
    );
    h.push_str("</body></html>");
    h
}

// ==================================================================== 测试

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::mock::MockDevice;

    fn base_dir(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("atraverse_script_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 造一个最小 L2 注册表
    fn write_kb(dir: &Path, body: &str) {
        let el = dir.join("elements");
        std::fs::create_dir_all(&el).unwrap();
        std::fs::write(el.join("demo.yaml"), body).unwrap();
    }

    const KB_DEMO: &str = r#"
screen: demo
elements:
- id: item_title
  semantic: 列表条目
  locators:
    - { type: resource_id, value: "com.demo:id/item_title" }
    - { type: bounds_center, value: "0.5000,0.1250" }
- id: back_btn
  semantic: 返回按钮
  locators:
    - { type: resource_id, value: "com.demo:id/back" }
- id: only_position
  semantic: 只有位置的控件
  locators:
    - { type: bounds_center, value: "0.5000,0.5000" }
    - { type: class_index, value: "FrameLayout#0" }
- id: detail_title
  semantic: 详情标题
  locators:
    - { type: text, value: "详情标题" }
"#;

    fn opts_for(kb: &Path, case: &Path, dry: bool) -> ScriptOptions {
        ScriptOptions {
            kb: kb.to_path_buf(),
            target: case.to_path_buf(),
            out: Some(kb.join("out")),
            package: None,
            settle_ms: 0,
            filter: None,
            dry_run: dry,
            verbose: false,
        }
    }

    fn write_case(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    // ---------------------------------------------------------- 闸门

    #[test]
    fn gate_blocks_unknown_element() {
        let d = base_dir("gate_unknown");
        write_kb(&d, KB_DEMO);
        let case = write_case(
            &d,
            "c.yaml",
            "id: T1\ntitle: 引用不存在的元素\nsteps:\n  - { n: 1, action: click, locator: { id: 不存在 } }\n\
assertions:\n  - { n: 2, type: no_crash }\n",
        );
        let reg = Registry::load(&d).unwrap();
        let c: Case = serde_yaml::from_str(&std::fs::read_to_string(&case).unwrap()).unwrap();
        let issues = gate_case(&c, &reg);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].reason.contains("L2 元素注册表"));
        assert_eq!(issues[0].location, "step 1");
    }

    #[test]
    fn gate_blocks_case_without_assertions() {
        let d = base_dir("gate_noassert");
        write_kb(&d, KB_DEMO);
        let case = write_case(
            &d,
            "c.yaml",
            "id: T0\nsteps:\n  - { n: 1, action: click, locator: { id: item_title } }\nassertions: []\n",
        );
        let reg = Registry::load(&d).unwrap();
        let c: Case = serde_yaml::from_str(&std::fs::read_to_string(&case).unwrap()).unwrap();
        let issues = gate_case(&c, &reg);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].reason.contains("没有任何断言"));
    }

    #[test]
    fn gate_blocks_existence_assertion_without_semantic_locator() {
        let d = base_dir("gate_weak");
        write_kb(&d, KB_DEMO);
        let case = write_case(
            &d,
            "c.yaml",
            "id: T2\nsteps: []\nassertions:\n  - { n: 1, type: element_exists, locator: { id: only_position } }\n",
        );
        let reg = Registry::load(&d).unwrap();
        let c: Case = serde_yaml::from_str(&std::fs::read_to_string(&case).unwrap()).unwrap();
        let issues = gate_case(&c, &reg);
        assert_eq!(issues.len(), 1, "只有位置/序号的元素不该被放行: {issues:?}");
        assert!(issues[0].reason.contains("假阳性"));
    }

    #[test]
    fn gate_passes_wellformed_case() {
        let d = base_dir("gate_ok");
        write_kb(&d, KB_DEMO);
        let case = write_case(
            &d,
            "c.yaml",
            "id: T3\nsteps:\n  - { n: 1, action: click, locator: { id: item_title } }\nassertions:\n  - { n: 2, type: text_visible, expect: 详情标题 }\n  - { n: 3, type: element_exists, locator: { id: detail_title } }\n",
        );
        let reg = Registry::load(&d).unwrap();
        let c: Case = serde_yaml::from_str(&std::fs::read_to_string(&case).unwrap()).unwrap();
        assert!(gate_case(&c, &reg).is_empty());
    }

    #[test]
    fn gate_blocks_unknown_assertion_and_action() {
        let d = base_dir("gate_unknown_kind");
        write_kb(&d, KB_DEMO);
        let case = write_case(
            &d,
            "c.yaml",
            "id: T4\nsteps:\n  - { n: 1, action: teleport }\nassertions:\n  - { n: 1, type: pixel_green }\n",
        );
        let reg = Registry::load(&d).unwrap();
        let c: Case = serde_yaml::from_str(&std::fs::read_to_string(&case).unwrap()).unwrap();
        let issues = gate_case(&c, &reg);
        assert_eq!(issues.len(), 2);
    }

    // ---------------------------------------------------------- 定位

    #[test]
    fn existence_lookup_ignores_positional_locators() {
        let d = base_dir("locate_pos");
        write_kb(&d, KB_DEMO);
        let reg = Registry::load(&d).unwrap();
        let hier = dump::parse_hierarchy(
            r#"<hierarchy rotation="0" width="540" height="1200">
<node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.demo" bounds="[0,0][540,1200]"/>
</hierarchy>"#,
        )
        .unwrap();
        let e = reg.get("only_position").unwrap();
        // 允许位置时能定位到（给动作使用）
        assert!(locate(e, &hier, true, true).is_some());
        // 存在性判断时不能（位置回答不了「在不在」）
        assert!(locate(e, &hier, false, false).is_none());
    }

    #[test]
    fn resource_id_wins_over_positional() {
        let d = base_dir("locate_order");
        write_kb(&d, KB_DEMO);
        let reg = Registry::load(&d).unwrap();
        let hier = dump::parse_hierarchy(
            r#"<hierarchy rotation="0" width="540" height="1200">
<node index="0" text="条目" resource-id="com.demo:id/item_title" class="android.widget.TextView" package="com.demo" bounds="[0,100][540,200]"/>
</hierarchy>"#,
        )
        .unwrap();
        let e = reg.get("item_title").unwrap();
        let (hit, loc) = locate(e, &hier, true, true).unwrap();
        assert_eq!(loc.kind, "resource_id");
        // 命中真实控件的中心，而不是 0.125*1200=150 的兜底点也算对：两者恰好一致
        assert_eq!(hit.center(), (270, 150));
    }

    #[test]
    fn content_desc_regex_is_start_anchored() {
        assert!(re_match(
            "^未点赞，喜欢[\\d.万亿]+，按钮$",
            "未点赞，喜欢5.4万，按钮"
        ));
        assert!(!re_match(
            "^未点赞，喜欢[\\d.万亿]+，按钮$",
            "x未点赞，喜欢5.4万，按钮"
        ));
        assert!(re_match("^[\\d.万亿]+$", "1.2万"));
        assert!(!re_match("^[\\d.万亿]+$", "1.2万赞"));
    }

    // ---------------------------------------------------------- 端到端（模拟设备）

    #[test]
    fn runs_case_end_to_end_on_mock_device() {
        let d = base_dir("e2e");
        write_kb(&d, KB_DEMO);
        let case = write_case(
            &d,
            "c.yaml",
            r#"
id: TC_E2E_001
title: 点列表项进详情，再返回
priority: P1
tags: [smoke]
trace: { requirement: REQ-1, pattern: navigation_tab, dimension: 正常流 }
steps:
  - { n: 1, action: click, locator: { id: item_title }, desc: 点第一条 }
assertions:
  - { n: 2, type: text_visible, expect: 详情标题, timeout_ms: 200 }
  - { n: 3, type: element_exists, locator: { id: back_btn }, timeout_ms: 200 }
  - { n: 4, type: text_equals, expect: 详情标题, timeout_ms: 200 }
teardown:
  - { action: click, locator: { id: back_btn } }
"#,
        );
        let mut dev = MockDevice::new(&d.join("mock"));
        let run = run(&opts_for(&d, &case, false), Some(&mut dev)).unwrap();
        assert_eq!(run.summary.pass, 1, "应通过: {:#?}", run.cases);
        assert_eq!(run.summary.fail, 0);
        // 点击生效：模拟设备已切到详情页
        assert_eq!(*dev.screen.lock().unwrap(), 0, "teardown 应已返回列表页");
        assert!(d.join("out").join("cases.json").exists());
        assert!(d.join("out").join("report.html").exists());
    }

    #[test]
    fn failing_assertion_reports_fail_not_pass() {
        let d = base_dir("e2e_fail");
        write_kb(&d, KB_DEMO);
        let case = write_case(
            &d,
            "c.yaml",
            "id: TC_E2E_002\ntitle: 期望一个列表页上没有的文本\nsteps: []\n\
assertions:\n  - { n: 1, type: text_visible, expect: 详情标题, timeout_ms: 0 }\n",
        );
        let mut dev = MockDevice::new(&d.join("mock"));
        let run = run(&opts_for(&d, &case, false), Some(&mut dev)).unwrap();
        assert_eq!(run.summary.fail, 1);
        assert_eq!(run.summary.pass, 0);
        let a = &run.cases[0].assertions[0];
        assert_eq!(a.status, AssertStatus::Fail);
        assert!(a.msg.contains("未找到文本"));
    }

    #[test]
    fn blocked_case_is_not_executed() {
        let d = base_dir("e2e_blocked");
        write_kb(&d, KB_DEMO);
        let case = write_case(
            &d,
            "c.yaml",
            "id: TC_E2E_003\nsteps:\n  - { n: 1, action: click, locator: { id: 幻觉元素 } }\n\
assertions:\n  - { n: 2, type: no_crash }\n",
        );
        let mut dev = MockDevice::new(&d.join("mock"));
        let run = run(&opts_for(&d, &case, false), Some(&mut dev)).unwrap();
        assert_eq!(run.summary.blocked, 1);
        assert!(dev.cmds.lock().unwrap().is_empty(), "阻塞的用例不该碰设备");
        assert!(run.cases[0].steps.is_empty());
    }

    #[test]
    fn dry_run_never_touches_device() {
        let d = base_dir("dry");
        write_kb(&d, KB_DEMO);
        let case = write_case(
            &d,
            "c.yaml",
            "id: TC_E2E_004\nsteps:\n  - { n: 1, action: click, locator: { id: item_title } }\n\
assertions:\n  - { n: 2, type: element_exists, locator: { id: back_btn } }\n",
        );
        let run = run(&opts_for(&d, &case, true), None).unwrap();
        assert_eq!(run.mode, "dry-run");
        assert_eq!(run.summary.pass, 1);
        // 报告照写（闸门报告是给人看的产物），但一律不碰设备
        assert!(d.join("out").join("report.html").exists());
        assert!(
            !d.join("out").join("logcat.txt").exists(),
            "dry-run 不该开崩溃监控"
        );
    }

    #[test]
    fn directory_mode_runs_all_cases_in_order() {
        let d = base_dir("suite");
        write_kb(&d, KB_DEMO);
        write_case(
            &d,
            "a.yaml",
            "id: TC_A\nsteps: []\nassertions:\n  - { n: 1, type: no_crash }\n",
        );
        write_case(
            &d,
            "b.yaml",
            "id: TC_B\nsteps: []\nassertions:\n  - { n: 1, type: element_exists, locator: { id: item_title } }\n",
        );
        let mut dev = MockDevice::new(&d.join("mock"));
        let mut o = opts_for(&d, &d, false);
        o.kb = d.clone();
        let run = run(&o, Some(&mut dev)).unwrap();
        assert_eq!(run.summary.total, 2);
        assert_eq!(run.cases[0].id, "TC_A");
        assert_eq!(run.cases[1].id, "TC_B");
        // 未指定 --package → no_crash 跳过：不算通过也不算失败（否则就是假 PASS）
        assert_eq!(run.cases[0].assertions[0].status, AssertStatus::Skip);
        assert_eq!(run.cases[0].status, "skip");
        assert_eq!(run.summary.skip, 1);
        assert_eq!(run.summary.skipped_assertions, 1);
        assert_eq!(run.summary.pass, 1, "只有 TC_B 真的验证过");
    }

    // ---------------------------------------------------------- no_crash 判定

    #[test]
    fn no_crash_mapping() {
        let ok = no_crash_outcome(true, &[]);
        assert!(ok.pass && !ok.skipped);

        let skipped = no_crash_outcome(false, &[]);
        assert!(skipped.skipped);

        let inc = Incident {
            kind: crate::monitor::IncidentKind::Crash,
            time: "12:00:00".to_string(),
            package: "com.demo".to_string(),
            summary: "java.lang.NullPointerException".to_string(),
            detail: String::new(),
            file: None,
            step: None,
            shot: None,
            shot_thumb: None,
        };
        let bad = no_crash_outcome(true, std::slice::from_ref(&inc));
        assert!(!bad.pass && !bad.skipped);
        assert!(bad.msg.contains("Java 崩溃"));
    }
}
