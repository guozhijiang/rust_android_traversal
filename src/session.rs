//! 会话数据：步骤记录、异常记录与整体 session 的持久化（JSON）。

use crate::device::DeviceInfo;
use crate::model::{Action, TargetInfo};
use crate::monitor::Incident;
use crate::strategy::{CoverageReport, StateRecord, Transition};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub index: usize,
    pub time: String,
    pub elapsed_ms: u64,
    pub action_kind: String,
    pub action_kind_cn: String,
    pub action_label: String,
    pub action_key: String,
    pub target: Option<TargetInfo>,
    pub point: Option<(i32, i32)>,
    pub points: Option<[(i32, i32); 2]>,
    pub text: Option<String>,
    pub activity_before: String,
    pub activity_after: String,
    pub state_before: u64,
    pub state_after: u64,
    pub new_state: bool,
    pub screenshot: Option<String>,
    pub thumb: Option<String>,
    pub duration_ms: u64,
    pub error: Option<String>,
}

impl StepRecord {
    pub fn from_action(
        index: usize,
        action: &Action,
        activity_before: &str,
        state_before: u64,
        elapsed_ms: u64,
    ) -> Self {
        let (point, points, text) = match action {
            Action::Click { point, .. }
            | Action::LongClick { point, .. }
            | Action::Input { point, .. } => (Some(*point), None, None),
            Action::Swipe { from, to, .. } => (None, Some([*from, *to]), None),
            _ => (None, None, None),
        };
        let text = match action {
            Action::Input { text, .. } => Some(text.clone()),
            _ => text,
        };
        let target = match action {
            Action::Click { target, .. }
            | Action::LongClick { target, .. }
            | Action::Input { target, .. } => Some(target.clone()),
            Action::Swipe { target, .. } => target.clone(),
            _ => None,
        };
        Self {
            index,
            time: crate::util::now_str(),
            elapsed_ms,
            action_kind: action.kind().to_string(),
            action_kind_cn: action.kind_cn().to_string(),
            action_label: action.label(),
            action_key: action.action_key(),
            target,
            point,
            points,
            text,
            activity_before: activity_before.to_string(),
            activity_after: String::new(),
            state_before,
            state_after: state_before,
            new_state: false,
            screenshot: None,
            thumb: None,
            duration_ms: 0,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunConfig {
    pub package: String,
    pub activity: Option<String>,
    pub duration_secs: u64,
    pub max_steps: usize,
    pub interval_ms: u64,
    pub seed: u64,
    pub screenshot: bool,
    pub annotate: bool,
    /// 状态指纹取法："structural"（默认，抵抗动态文本）或 "exact"
    #[serde(default = "default_state_mode")]
    pub state_mode: String,
}

fn default_state_mode() -> String {
    "structural".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub package: String,
    pub activity: String,
    pub device: DeviceInfo,
    pub config: RunConfig,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    pub steps: Vec<StepRecord>,
    pub incidents: Vec<Incident>,
    pub coverage: CoverageReport,
    /// 状态迁移图：某一步的动作把状态从 from 带到了 to
    pub transitions: Vec<Transition>,
    /// 状态明细（含每个状态的控件签名）。
    ///
    /// `state_count` 只是数量；这里把状态本身写出来，下游才能做页面归并 ——
    /// 状态 id 是整棵树的结构指纹，动态内容应用上同一页面会产生很多 id，
    /// 必须靠 `StateRecord::keys` 把语义相同的状态合并回一个页面。
    #[serde(default)]
    pub states: Vec<StateRecord>,
    pub state_count: usize,
    pub version: String,
}

impl Session {
    pub fn crash_count(&self) -> usize {
        self.incidents
            .iter()
            .filter(|i| {
                matches!(
                    i.kind,
                    crate::monitor::IncidentKind::Crash | crate::monitor::IncidentKind::NativeCrash
                )
            })
            .count()
    }
    pub fn anr_count(&self) -> usize {
        self.incidents
            .iter()
            .filter(|i| matches!(i.kind, crate::monitor::IncidentKind::Anr))
            .count()
    }
}

/// 追加一行步骤（崩溃中断时也不丢数据）
pub fn append_step(path: &Path, step: &StepRecord) -> Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut f, step)?;
    writeln!(f)?;
    f.flush()?;
    Ok(())
}

pub fn write_session(dir: &Path, session: &Session) -> Result<()> {
    let mut f = File::create(dir.join("session.json"))?;
    serde_json::to_writer_pretty(&mut f, session)?;
    writeln!(f)?;
    Ok(())
}

pub fn read_session(dir: &Path) -> Result<Session> {
    let f = File::open(dir.join("session.json"))?;
    Ok(serde_json::from_reader(f)?)
}
