use std::collections::BTreeMap;
use std::io::ErrorKind;

use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::ApplyPatchHandler;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

const MAX_READ_LINES: usize = 2_000;

#[derive(Clone, Copy)]
pub struct ReadFileHandler {
    include_environment_id: bool,
}

#[derive(Clone, Copy)]
pub struct WriteFileHandler {
    include_environment_id: bool,
}

#[derive(Clone, Copy)]
pub struct UpdateFileHandler {
    include_environment_id: bool,
}

impl ReadFileHandler {
    pub(crate) fn new(include_environment_id: bool) -> Self {
        Self {
            include_environment_id,
        }
    }
}

impl WriteFileHandler {
    pub(crate) fn new(include_environment_id: bool) -> Self {
        Self {
            include_environment_id,
        }
    }
}

impl UpdateFileHandler {
    pub(crate) fn new(include_environment_id: bool) -> Self {
        Self {
            include_environment_id,
        }
    }
}

#[derive(Deserialize)]
struct ReadFileArgs {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Deserialize)]
struct UpdateFileArgs {
    path: String,
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    environment_id: Option<String>,
}

fn environment_property(properties: &mut BTreeMap<String, JsonSchema>, include: bool) {
    if include {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "Environment id from <environment_context>; omit for the primary environment."
                    .to_string(),
            )),
        );
    }
}

fn function_spec(
    name: &str,
    description: &str,
    properties: BTreeMap<String, JsonSchema>,
    required: Vec<&str>,
) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: name.to_string(),
        description: description.to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(required.into_iter().map(str::to_string).collect()),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

impl ToolExecutor<ToolInvocation> for ReadFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("read_file")
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::from([
            (
                "path".to_string(),
                JsonSchema::string(Some(
                    "Filesystem path, resolved relative to the environment working directory."
                        .to_string(),
                )),
            ),
            (
                "start_line".to_string(),
                JsonSchema::integer(Some(
                    "First line to return, using one-based inclusive numbering; defaults to 1."
                        .to_string(),
                )),
            ),
            (
                "end_line".to_string(),
                JsonSchema::integer(Some(
                    "Last line to return, using one-based inclusive numbering; omit for EOF."
                        .to_string(),
                )),
            ),
        ]);
        environment_property(&mut properties, self.include_environment_id);
        function_spec(
            "read_file",
            "Read a UTF-8 text file with stable one-based line numbers. Use start_line and end_line for precise ranged reads; responses are capped at 2,000 lines.",
            properties,
            vec!["path"],
        )
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "read_file handler received unsupported payload".to_string(),
                ));
            };
            let args: ReadFileArgs = parse_arguments(arguments)?;
            let start = args.start_line.unwrap_or(1);
            if start == 0 {
                return Err(FunctionCallError::RespondToModel(
                    "start_line must be at least 1".to_string(),
                ));
            }
            if args.end_line.is_some_and(|end| end < start) {
                return Err(FunctionCallError::RespondToModel(
                    "end_line must be greater than or equal to start_line".to_string(),
                ));
            }

            let turn_environment = resolve_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
            )?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "read_file is unavailable in this session".to_string(),
                )
            })?;
            let path = turn_environment.cwd().join(&args.path).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "unable to resolve `{}` against `{}`: {err}",
                    args.path,
                    turn_environment.cwd()
                ))
            })?;
            let sandbox = invocation.turn.file_system_sandbox_context(
                /*additional_permissions*/ None,
                turn_environment.cwd(),
            );
            let contents = turn_environment
                .environment
                .get_filesystem()
                .read_file_text(&path, Some(&sandbox))
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "unable to read `{}`: {err}",
                        path.inferred_native_path_string()
                    ))
                })?;
            let lines = contents.lines().collect::<Vec<_>>();
            if lines.is_empty() {
                if start != 1 {
                    return Err(FunctionCallError::RespondToModel(
                        "start_line is past EOF (empty file)".to_string(),
                    ));
                }
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    format!("{} — empty file", path.inferred_native_path_string()),
                    Some(true),
                )));
            }
            if start > lines.len() {
                return Err(FunctionCallError::RespondToModel(format!(
                    "start_line {start} is past EOF ({} lines)",
                    lines.len()
                )));
            }
            let requested_end = args.end_line.unwrap_or(lines.len()).min(lines.len());
            let capped_end = requested_end.min(start.saturating_add(MAX_READ_LINES - 1));
            let mut output = format!(
                "{} — lines {start}-{capped_end} of {}\n",
                path.inferred_native_path_string(),
                lines.len()
            );
            for (index, line) in lines.iter().enumerate().take(capped_end).skip(start - 1) {
                output.push_str(&format!("{:>6}\t{line}\n", index + 1));
            }
            if capped_end < requested_end {
                output.push_str(&format!(
                    "[truncated at {MAX_READ_LINES} lines; continue from line {}]",
                    capped_end + 1
                ));
            }
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                output,
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for ReadFileHandler {}

impl ToolExecutor<ToolInvocation> for WriteFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("write_file")
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::from([
            (
                "path".to_string(),
                JsonSchema::string(Some("Path for the new text file.".to_string())),
            ),
            (
                "content".to_string(),
                JsonSchema::string(Some("Complete UTF-8 file content.".to_string())),
            ),
        ]);
        environment_property(&mut properties, self.include_environment_id);
        function_spec(
            "write_file",
            "Create a new UTF-8 text file through Codex's patch and approval pipeline. Fails if the file already exists; use update_file or apply_patch for existing files.",
            properties,
            vec!["path", "content"],
        )
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        let include_environment_id = self.include_environment_id;
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "write_file handler received unsupported payload".to_string(),
                ));
            };
            let args: WriteFileArgs = parse_arguments(arguments)?;
            validate_patch_path(&args.path)?;
            let turn_environment = resolve_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
            )?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "write_file is unavailable in this session".to_string(),
                )
            })?;
            let path = turn_environment.cwd().join(&args.path).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "unable to resolve `{}`: {err}",
                    args.path
                ))
            })?;
            let sandbox = invocation.turn.file_system_sandbox_context(
                /*additional_permissions*/ None,
                turn_environment.cwd(),
            );
            match turn_environment
                .environment
                .get_filesystem()
                .get_metadata(&path, Some(&sandbox))
                .await
            {
                Ok(_) => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "`{}` already exists; use update_file or apply_patch",
                        path.inferred_native_path_string()
                    )));
                }
                Err(err) if err.kind() == ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "unable to inspect `{}`: {err}",
                        path.inferred_native_path_string()
                    )));
                }
            }

            delegate_patch(
                invocation,
                create_add_patch(&args.path, &args.content, args.environment_id.as_deref()),
                include_environment_id,
            )
            .await
        })
    }
}

impl CoreToolRuntime for WriteFileHandler {}

impl ToolExecutor<ToolInvocation> for UpdateFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("update_file")
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::from([
            (
                "path".to_string(),
                JsonSchema::string(Some("Path to an existing UTF-8 text file.".to_string())),
            ),
            (
                "old_text".to_string(),
                JsonSchema::string(Some(
                    "Exact existing text to replace; must match once unless replace_all is true."
                        .to_string(),
                )),
            ),
            (
                "new_text".to_string(),
                JsonSchema::string(Some("Replacement text.".to_string())),
            ),
            (
                "replace_all".to_string(),
                JsonSchema::boolean(Some(
                    "Replace every exact match; defaults to false.".to_string(),
                )),
            ),
        ]);
        environment_property(&mut properties, self.include_environment_id);
        function_spec(
            "update_file",
            "Update an existing UTF-8 file by exact text replacement through Codex's patch and approval pipeline. Use apply_patch for multi-file or structural edits.",
            properties,
            vec!["path", "old_text", "new_text"],
        )
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        let include_environment_id = self.include_environment_id;
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "update_file handler received unsupported payload".to_string(),
                ));
            };
            let args: UpdateFileArgs = parse_arguments(arguments)?;
            validate_patch_path(&args.path)?;
            if args.old_text.is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "old_text must not be empty".to_string(),
                ));
            }
            let turn_environment = resolve_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
            )?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "update_file is unavailable in this session".to_string(),
                )
            })?;
            let path = turn_environment.cwd().join(&args.path).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "unable to resolve `{}`: {err}",
                    args.path
                ))
            })?;
            let sandbox = invocation.turn.file_system_sandbox_context(
                /*additional_permissions*/ None,
                turn_environment.cwd(),
            );
            let old_contents = turn_environment
                .environment
                .get_filesystem()
                .read_file_text(&path, Some(&sandbox))
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "unable to read `{}`: {err}",
                        path.inferred_native_path_string()
                    ))
                })?;
            let matches = old_contents.match_indices(&args.old_text).count();
            if matches == 0 {
                return Err(FunctionCallError::RespondToModel(
                    "old_text was not found in the file".to_string(),
                ));
            }
            if matches > 1 && !args.replace_all {
                return Err(FunctionCallError::RespondToModel(format!(
                    "old_text matched {matches} times; provide more context or set replace_all"
                )));
            }
            let new_contents = if args.replace_all {
                old_contents.replace(&args.old_text, &args.new_text)
            } else {
                old_contents.replacen(&args.old_text, &args.new_text, 1)
            };

            delegate_patch(
                invocation,
                create_update_patch(
                    &args.path,
                    &old_contents,
                    &new_contents,
                    args.environment_id.as_deref(),
                ),
                include_environment_id,
            )
            .await
        })
    }
}

impl CoreToolRuntime for UpdateFileHandler {}

fn validate_patch_path(path: &str) -> Result<(), FunctionCallError> {
    if path.is_empty() || path.contains(['\n', '\r']) {
        return Err(FunctionCallError::RespondToModel(
            "path must be non-empty and contain no newlines".to_string(),
        ));
    }
    Ok(())
}

fn patch_header(environment_id: Option<&str>) -> String {
    let mut patch = "*** Begin Patch\n".to_string();
    if let Some(environment_id) = environment_id {
        patch.push_str(&format!("*** Environment ID: {environment_id}\n"));
    }
    patch
}

fn normalized_lines(content: &str) -> Vec<&str> {
    let mut lines = content.split('\n').collect::<Vec<_>>();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

fn create_add_patch(path: &str, content: &str, environment_id: Option<&str>) -> String {
    let mut patch = patch_header(environment_id);
    patch.push_str(&format!("*** Add File: {path}\n"));
    let lines = normalized_lines(content);
    if lines.is_empty() {
        patch.push_str("+\n");
    } else {
        for line in lines {
            patch.push('+');
            patch.push_str(line.trim_end_matches('\r'));
            patch.push('\n');
        }
    }
    patch.push_str("*** End Patch");
    patch
}

fn create_update_patch(
    path: &str,
    old_content: &str,
    new_content: &str,
    environment_id: Option<&str>,
) -> String {
    let mut patch = patch_header(environment_id);
    patch.push_str(&format!("*** Update File: {path}\n@@\n"));
    for line in normalized_lines(old_content) {
        patch.push('-');
        patch.push_str(line.trim_end_matches('\r'));
        patch.push('\n');
    }
    for line in normalized_lines(new_content) {
        patch.push('+');
        patch.push_str(line.trim_end_matches('\r'));
        patch.push('\n');
    }
    patch.push_str("*** End of File\n*** End Patch");
    patch
}

async fn delegate_patch(
    mut invocation: ToolInvocation,
    patch: String,
    include_environment_id: bool,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    invocation.payload = ToolPayload::Custom { input: patch };
    ApplyPatchHandler::new(include_environment_id)
        .handle(invocation)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_patch_preserves_content_lines() {
        let patch = create_add_patch("new.txt", "one\ntwo\n", None);
        assert_eq!(
            patch,
            "*** Begin Patch\n*** Add File: new.txt\n+one\n+two\n*** End Patch"
        );
        assert!(codex_apply_patch::parse_patch(&patch).is_ok());
    }

    #[test]
    fn update_patch_includes_environment_and_full_replacement() {
        let patch = create_update_patch("a.txt", "old\n", "new\n", Some("remote"));
        assert_eq!(
            patch,
            "*** Begin Patch\n*** Environment ID: remote\n*** Update File: a.txt\n@@\n-old\n+new\n*** End of File\n*** End Patch"
        );
        assert!(codex_apply_patch::parse_patch(&patch).is_ok());
    }
}
