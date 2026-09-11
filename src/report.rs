//! HTML 报告生成：概览指标 / 异常面板 / Activity 覆盖率 / 操作步骤时间线。

use crate::session::Session;
use crate::util::fmt_duration;
use anyhow::Result;
use std::fmt::Write as _;
use std::path::Path;

const TEMPLATE: &str = include_str!("report_template.html");

pub fn render(session: &Session, out_dir: &Path) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(out_dir)?;
    let html = build_html(session);
    let path = out_dir.join("index.html");
    std::fs::write(&path, html)?;
    Ok(path)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn pct(v: f32) -> String {
    format!("{:.1}%", v * 100.0)
}

fn build_html(s: &Session) -> String {
    let cov = &s.coverage;
    let crashes = s.crash_count();
    let anrs = s.anr_count();

    // ---------- 指标卡
    let mut metrics = String::new();
    let shot_count = s.steps.iter().filter(|x| x.screenshot.is_some()).count();
    let node_sub = format!("{} / {}", cov.touched_nodes, cov.total_nodes);
    let inc_sub = format!("crash {} / anr {}", crashes, anrs);
    let cards: Vec<(&str, String, &str, &str)> = vec![
        ("执行步骤", s.steps.len().to_string(), "步", ""),
        ("Activity 覆盖", cov.activity_count.to_string(), "个", "accent"),
        ("状态去重", s.state_count.to_string(), "个", ""),
        (
            "控件覆盖率",
            pct(cov.node_coverage),
            node_sub.as_str(),
            if cov.node_coverage >= 0.6 {
                "ok"
            } else if cov.node_coverage >= 0.3 {
                "warn"
            } else {
                "bad"
            },
        ),
        ("截图", shot_count.to_string(), "张", ""),
        (
            "异常",
            s.incidents.len().to_string(),
            inc_sub.as_str(),
            if s.incidents.is_empty() { "ok" } else { "bad" },
        ),
    ];
    for (title, value, sub, cls) in cards {
        let _ = write!(
            metrics,
            r#"<div class="card {}"><div class="card-t">{}</div><div class="card-v">{}</div><div class="card-s">{}</div></div>"#,
            cls, esc(title), esc(&value), esc(sub)
        );
    }

    // ---------- 异常面板
    let mut incidents = String::new();
    if s.incidents.is_empty() {
        incidents.push_str(
            r#"<div class="empty">本次遍历未发现 crash / ANR 异常。</div>"#,
        );
    } else {
        for (i, inc) in s.incidents.iter().enumerate() {
            let kind_cls = match inc.kind {
                crate::monitor::IncidentKind::Crash => "tag-crash",
                crate::monitor::IncidentKind::NativeCrash => "tag-crash",
                crate::monitor::IncidentKind::Anr => "tag-anr",
            };
            let file_html = match &inc.file {
                Some(f) => format!(
                    r#"<a class="lnk" href="../{}" target="_blank">查看完整日志</a>"#,
                    esc(f)
                ),
                None => String::new(),
            };
            let step_html = match inc.step {
                Some(st) => format!("<span class=\"chip\">步骤 #{}</span>", st),
                None => String::new(),
            };
            let _ = write!(
                incidents,
                r#"<details class="inc" {}><summary><span class="tag {}">{}</span><span class="inc-sum">{}</span><span class="inc-time">{}</span>{}</summary><pre class="stack">{}</pre>{}</details>"#,
                if i == 0 { "open" } else { "" },
                kind_cls,
                esc(&inc.kind_cn()),
                esc(&inc.summary),
                esc(&inc.time),
                step_html,
                esc(&inc.detail),
                file_html
            );
        }
    }

    // ---------- Activity 覆盖表
    let mut cov_rows = String::new();
    if cov.activities.is_empty() {
        cov_rows.push_str(r#"<tr><td colspan="6" class="empty">未采集到被测应用的 Activity</td></tr>"#);
    }
    for a in &cov.activities {
        let cls = if a.coverage >= 0.6 {
            "ok"
        } else if a.coverage >= 0.3 {
            "warn"
        } else {
            "bad"
        };
        let _ = write!(
            cov_rows,
            r#"<tr>
<td class="mono">{}</td>
<td class="num">{}</td>
<td class="num">#{}</td>
<td class="num">{}</td>
<td class="num">{}</td>
<td><div class="bar"><div class="bar-in {} " style="width:{}%"></div></div><span class="pct {} ">{}</span></td>
</tr>"#,
            esc(&a.name),
            a.visits,
            a.first_step,
            a.nodes_seen,
            a.nodes_touched,
            cls,
            (a.coverage * 100.0).clamp(0.0, 100.0),
            cls,
            pct(a.coverage)
        );
    }

    // 未覆盖控件
    let mut untouched = String::new();
    if !cov.untouched_sample.is_empty() {
        let mut rows = String::new();
        for n in &cov.untouched_sample {
            let _ = write!(
                rows,
                r#"<tr><td class="mono">{}</td><td>{}</td><td class="mono dim">{}</td><td class="num">{}</td></tr>"#,
                esc(&n.class),
                esc(&n.label),
                esc(&n.resource_id),
                n.seen
            );
        }
        untouched = format!(
            r#"<details class="more"><summary>未被操作过的控件（最多 50 个，共 {} 个）</summary><table class="tbl"><thead><tr><th>类型</th><th>标识</th><th>resource-id</th><th>出现次数</th></tr></thead><tbody>{}</tbody></table></details>"#,
            cov.total_nodes - cov.touched_nodes,
            rows
        );
    }

    // ---------- 时间线
    let mut timeline = String::new();
    for st in &s.steps {
        let img = match (&st.thumb, &st.screenshot) {
            (Some(t), Some(full)) => format!(
                r#"<a class="shot" href="../{}" target="_blank"><img loading="lazy" src="../{}" alt="step {}"></a>"#,
                esc(full),
                esc(t),
                st.index
            ),
            (_, Some(full)) => format!(
                r#"<a class="shot" href="../{}" target="_blank"><img loading="lazy" src="../{}" alt="step {}"></a>"#,
                esc(full),
                esc(full),
                st.index
            ),
            _ => r#"<div class="shot none">无截图</div>"#.to_string(),
        };
        let target_html = match &st.target {
            Some(t) => format!(
                r#"<div class="t-target"><span class="chip">{}</span><span class="mono dim">{}</span></div>"#,
                esc(&t.label),
                esc(&t.resource_id)
            ),
            None => String::new(),
        };
        let err_html = match &st.error {
            Some(e) => format!(r#"<div class="t-err">执行失败：{}</div>"#, esc(e)),
            None => String::new(),
        };
        let new_state = if st.new_state {
            r#"<span class="chip new">新状态</span>"#.to_string()
        } else {
            String::new()
        };
        let act_change = if !st.activity_after.is_empty() && st.activity_after != st.activity_before {
            format!(
                r#"<div class="t-act"><span class="mono dim">{}</span> → <span class="mono">{}</span></div>"#,
                esc(&st.activity_before),
                esc(&st.activity_after)
            )
        } else {
            format!(
                r#"<div class="t-act"><span class="mono dim">{}</span></div>"#,
                esc(&st.activity_before)
            )
        };
        let _ = write!(
            timeline,
            r#"<div class="step" data-kind="{}">
  <div class="t-head"><span class="t-idx">#{}</span><span class="t-kind k-{}">{}</span>{}
    <span class="t-time">{} · {}ms</span></div>
  <div class="t-body">{}<div class="t-main"><div class="t-label">{}</div>{}{}{}</div></div>
</div>"#,
            esc(&st.action_kind),
            st.index,
            esc(&st.action_kind),
            esc(&st.action_kind_cn),
            new_state,
            esc(&st.time),
            st.duration_ms,
            img,
            esc(&st.action_label),
            target_html,
            act_change,
            err_html
        );
    }
    if s.steps.is_empty() {
        timeline.push_str(r#"<div class="empty">没有记录到任何步骤。</div>"#);
    }

    // ---------- 组装
    let mut kinds: Vec<String> = s
        .steps
        .iter()
        .map(|x| x.action_kind.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    kinds.sort();
    let filter_btns: String = std::iter::once(r#"<button class="fbtn active" data-f="all">全部</button>"#.to_string())
        .chain(kinds.iter().map(|k| {
            let cn = s
                .steps
                .iter()
                .find(|x| &x.action_kind == k)
                .map(|x| x.action_kind_cn.clone())
                .unwrap_or_else(|| k.clone());
            format!(r#"<button class="fbtn" data-f="{}">{}</button>"#, esc(k), esc(&cn))
        }))
        .collect::<Vec<_>>()
        .join("\n");

    let html = TEMPLATE
        .replace("__PACKAGE__", &esc(&s.package))
        .replace("__DEVICE__", &esc(&s.device.display()))
        .replace("__STARTED__", &esc(&s.started_at))
        .replace("__FINISHED__", &esc(&s.finished_at))
        .replace("__DURATION__", &fmt_duration(s.duration_ms))
        .replace("__METRICS__", &metrics)
        .replace("__INCIDENTS__", &incidents)
        .replace("__COVERAGE__", &cov_rows)
        .replace("__UNTOUCHED__", &untouched)
        .replace("__TIMELINE__", &timeline)
        .replace("__FILTERS__", &filter_btns)
        .replace("__VERSION__", &esc(&s.version));
    html
}
