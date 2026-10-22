//! Task-scoped plan presentation; execution evidence remains in tool results.
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use smallhead_core::plan::{PlanStatus, TaskPlan};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanPhase {
    Running,
    Completed,
    Incomplete,
    Interrupted,
}

impl PlanPhase {
    fn label(self) -> &'static str {
        match self {
            Self::Running => "执行中",
            Self::Completed => "已完成",
            Self::Incomplete => "尚未完成",
            Self::Interrupted => "已中断",
        }
    }
}

pub struct PlanView {
    pub snapshot: TaskPlan,
    pub phase: PlanPhase,
}

impl PlanView {
    pub fn completed(&self) -> usize {
        self.snapshot
            .plan
            .iter()
            .filter(|s| s.status == PlanStatus::Completed)
            .count()
    }

    pub fn finish(&mut self, interrupted: bool) {
        self.phase = if interrupted {
            PlanPhase::Interrupted
        } else if self.completed() == self.snapshot.plan.len() {
            PlanPhase::Completed
        } else {
            PlanPhase::Incomplete
        };
    }

    pub fn title(&self) -> String {
        format!(
            "任务计划 {}/{} · {}",
            self.completed(),
            self.snapshot.plan.len(),
            self.phase.label()
        )
    }

    pub fn full_text(&self) -> String {
        let mut lines = vec![self.title()];
        if let Some(explanation) = &self.snapshot.explanation {
            lines.push(explanation.clone());
        }
        lines.extend(
            self.snapshot
                .plan
                .iter()
                .enumerate()
                .map(|(i, step)| format!("{} {}. {}", marker(step.status), i + 1, step.step)),
        );
        lines.join("\n")
    }
}

fn marker(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::Pending => "[ ]",
        PlanStatus::InProgress => "[>]",
        PlanStatus::Completed => "[x]",
    }
}

pub fn draw(f: &mut Frame, plan: &PlanView, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(plan.title());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let capacity = inner.height as usize;
    if capacity == 0 {
        return;
    }
    let steps = &plan.snapshot.plan;
    // Keep the active (or first pending) step visible when a plan exceeds the panel.
    let active = steps
        .iter()
        .position(|s| s.status == PlanStatus::InProgress)
        .or_else(|| steps.iter().position(|s| s.status == PlanStatus::Pending))
        .unwrap_or(steps.len().saturating_sub(1));
    let hidden = steps.len() > capacity;
    let visible = if hidden {
        capacity.saturating_sub(1).max(1)
    } else {
        capacity
    };
    let start = active
        .saturating_sub(visible / 2)
        .min(steps.len().saturating_sub(visible));
    let mut lines: Vec<Line<'static>> = steps
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, step)| {
            let color = match step.status {
                PlanStatus::Pending => Color::Gray,
                PlanStatus::InProgress => Color::Yellow,
                PlanStatus::Completed => Color::Green,
            };
            Line::from(Span::styled(
                format!(
                    " {} {}. {}",
                    marker(step.status),
                    i + 1,
                    step.step.replace(['\n', '\r'], " ")
                ),
                Style::default().fg(color),
            ))
        })
        .collect();
    if hidden && capacity > 1 {
        lines.push(Line::styled(
            format!(
                " {start} 项在上方 · {} 项在下方 · /plan",
                steps.len().saturating_sub(start + visible)
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(lines), inner);
}
