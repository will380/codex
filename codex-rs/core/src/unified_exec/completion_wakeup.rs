use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use codex_context_fragments::ContextualUserFragment;
use codex_utils_output_truncation::TruncationPolicy;
use tokio::sync::Mutex;

use crate::context::ExecCommandCompletion;
use crate::session::session::Session;
use crate::tools::context::ExecCommandToolOutput;
use crate::unified_exec::UnifiedExecProcess;
use crate::unified_exec::generate_chunk_id;

#[derive(Clone)]
pub(crate) struct CompletionWakeRegistration(Arc<CompletionWakeRegistrationInner>);

struct CompletionWakeRegistrationInner {
    state: Mutex<CompletionWakeState>,
    process: Arc<UnifiedExecProcess>,
    session: Weak<Session>,
    call_id: String,
    process_id: i32,
    command: String,
    max_output_tokens: Option<usize>,
    truncation_policy: TruncationPolicy,
}

#[derive(Default)]
struct CompletionWakeState {
    armed: bool,
    completed: Option<(i32, Duration)>,
    observed: bool,
}

impl CompletionWakeRegistration {
    pub(crate) fn new(
        process: Arc<UnifiedExecProcess>,
        session: Weak<Session>,
        call_id: String,
        process_id: i32,
        command: String,
        max_output_tokens: Option<usize>,
        truncation_policy: TruncationPolicy,
    ) -> Self {
        Self(Arc::new(CompletionWakeRegistrationInner {
            state: Mutex::new(CompletionWakeState::default()),
            process,
            session,
            call_id,
            process_id,
            command,
            max_output_tokens,
            truncation_policy,
        }))
    }

    pub(crate) async fn arm(&self) {
        let completed = {
            let mut state = self.0.state.lock().await;
            state.armed = true;
            state.completed.take().filter(|_| !state.observed)
        };
        if let Some((exit_code, duration)) = completed {
            self.enqueue(exit_code, duration).await;
        }
    }

    pub(crate) async fn complete(&self, exit_code: i32, duration: Duration) {
        let should_enqueue = {
            let mut state = self.0.state.lock().await;
            if state.observed {
                false
            } else if state.armed {
                true
            } else {
                state.completed = Some((exit_code, duration));
                false
            }
        };
        if should_enqueue {
            self.enqueue(exit_code, duration).await;
        }
    }

    pub(crate) async fn observed(&self) {
        self.0.state.lock().await.observed = true;
        if let Some(session) = self.0.session.upgrade() {
            session
                .input_queue
                .remove_exec_wakeup(self.0.process_id)
                .await;
        }
    }

    async fn enqueue(&self, exit_code: i32, duration: Duration) {
        let Some(session) = self.0.session.upgrade() else {
            return;
        };
        let drained = self.0.process.drain_output().await;
        let output = ExecCommandToolOutput {
            event_call_id: self.0.call_id.clone(),
            chunk_id: generate_chunk_id(),
            wall_time: duration,
            raw_output: drained.to_bytes_with_omission_marker(),
            truncation_policy: self.0.truncation_policy,
            max_output_tokens: self.0.max_output_tokens,
            process_id: None,
            exit_code: Some(exit_code),
            original_token_count: None,
            output_omitted_bytes: std::num::NonZeroUsize::new(drained.omitted_bytes()),
            hook_command: None,
            completion_notification: None,
        };
        let fragment = ExecCommandCompletion::new(
            self.0.call_id.clone(),
            self.0.process_id,
            self.0.command.clone(),
            exit_code,
            duration,
            output.model_output(),
        );
        session
            .input_queue
            .enqueue_exec_wakeup(self.0.process_id, ContextualUserFragment::into(fragment))
            .await;
        if self.0.state.lock().await.observed {
            session
                .input_queue
                .remove_exec_wakeup(self.0.process_id)
                .await;
            return;
        }
        session.schedule_exec_wakeup();
    }
}
