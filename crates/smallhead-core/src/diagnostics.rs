//! Scoped diagnostics for interactive clients. No HTTP headers or credentials are recorded.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

/// Structured diagnostic record; ordering is assigned by the consuming UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    /// Unix milliseconds.
    pub timestamp_ms: u64,
    /// Logical layer: request, message, block, tool, monad, route, response.
    pub layer: String,
    /// Operation within the layer.
    pub event: String,
    /// Complete structured payload (not a truncated preview).
    pub data: Value,
}

tokio::task_local! {
    static SINK: mpsc::UnboundedSender<DiagnosticEvent>;
    static MONAD_SCOPE: (String, u32);
}

/// Keep child events identifiable without rendering their tokens as the parent's
/// answer. This adds no forwarding queue and preserves full diagnostic payloads.
pub async fn in_monad<F: std::future::Future>(
    task_id: String,
    attempt_id: u32,
    future: F,
) -> F::Output {
    MONAD_SCOPE.scope((task_id, attempt_id), future).await
}

/// Capture task-local diagnostics before spawning a managed worker. Tokio does
/// not inherit task locals automatically. No process-global observer is used.
pub fn inherit<F: std::future::Future>(future: F) -> impl std::future::Future<Output = F::Output> {
    let sink = SINK.try_with(Clone::clone).ok();
    let scope = MONAD_SCOPE.try_with(Clone::clone).ok();
    async move {
        let scoped = async move {
            match scope {
                Some(scope) => MONAD_SCOPE.scope(scope, future).await,
                None => future.await,
            }
        };
        match sink {
            Some(sink) => SINK.scope(sink, scoped).await,
            None => scoped.await,
        }
    }
}

/// Install a per-task observer for the supplied future; independent tasks do not share sinks.
pub async fn observe<F: std::future::Future>(
    sink: mpsc::UnboundedSender<DiagnosticEvent>,
    future: F,
) -> F::Output {
    SINK.scope(sink, future).await
}

/// Emit if an observer is installed. Returns whether the event reached a receiver.
pub fn emit(layer: &str, event: &str, data: Value) -> bool {
    let scoped = MONAD_SCOPE
        .try_with(|(task_id, attempt_id)| {
            serde_json::json!({"task_id":task_id,"attempt_id":attempt_id,
            "layer":layer,"event":event,"data":data})
        })
        .ok();
    let (layer, event, data) = match scoped {
        Some(data) => ("monad", "child_event", data),
        None => (layer, event, data),
    };
    SINK.try_with(|sink| {
        sink.send(DiagnosticEvent {
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            layer: layer.into(),
            event: event.into(),
            data,
        })
        .is_ok()
    })
    .unwrap_or(false)
}

/// Keep standalone demos readable, but never print into a UI observing this task.
pub fn demo_event(layer: &str, event: &str, data: Value) {
    let observed = SINK.try_with(|_| ()).is_ok();
    if observed {
        emit(layer, event, data);
    } else {
        println!("  [{event}] {data}");
    }
}
