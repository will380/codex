use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::session::Session;

const EXEC_WAKEUP_BATCH_WINDOW: Duration = Duration::from_secs(5);

impl Session {
    pub(crate) fn schedule_exec_wakeup(self: &Arc<Self>) {
        if self
            .input_queue
            .exec_wakeup_timer_running
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        let session = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(EXEC_WAKEUP_BATCH_WINDOW).await;
            session
                .input_queue
                .exec_wakeup_timer_running
                .store(false, Ordering::Release);
            session.dispatch_exec_wakeups().await;
        });
    }

    async fn dispatch_exec_wakeups(self: &Arc<Self>) {
        if !self.input_queue.has_exec_wakeups().await {
            return;
        }
        if self.active_turn.lock().await.is_some()
            || self.input_queue.has_trigger_turn_mailbox_items().await
        {
            self.schedule_exec_wakeup();
            return;
        }
        let input = self.input_queue.drain_exec_wakeups().await;
        if let Err(err) = self.try_start_turn_if_idle(input).await {
            for (index, item) in err.into_input().into_iter().enumerate() {
                self.input_queue
                    .enqueue_exec_wakeup(i32::MIN.saturating_add(index as i32), item)
                    .await;
            }
            self.schedule_exec_wakeup();
        }
    }
}
