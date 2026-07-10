use std::time::Duration;

use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExecCommandCompletion {
    pub(crate) call_id: String,
    pub(crate) session_id: i32,
    pub(crate) command: String,
    pub(crate) exit_code: i32,
    pub(crate) duration_seconds: f64,
    pub(crate) output: String,
}

impl ExecCommandCompletion {
    pub(crate) fn new(
        call_id: String,
        session_id: i32,
        command: String,
        exit_code: i32,
        duration: Duration,
        output: String,
    ) -> Self {
        Self {
            call_id,
            session_id,
            command,
            exit_code,
            duration_seconds: duration.as_secs_f64(),
            output,
        }
    }
}

impl ContextualUserFragment for ExecCommandCompletion {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<exec_command_completion>", "</exec_command_completion>")
    }

    fn body(&self) -> String {
        format!(
            "\n<call_id>{}</call_id>\n<session_id>{}</session_id>\n<command>{}</command>\n<exit_code>{}</exit_code>\n<duration_seconds>{:.4}</duration_seconds>\n<output>\n{}\n</output>\n",
            self.call_id,
            self.session_id,
            self.command,
            self.exit_code,
            self.duration_seconds,
            self.output,
        )
    }
}
