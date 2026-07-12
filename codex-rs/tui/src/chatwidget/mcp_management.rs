//! Interactive MCP server management for the `/mcp` command.

use super::*;
use crate::app_event::McpInventoryPresentation;
use codex_app_server_protocol::McpAuthStatus;
use codex_app_server_protocol::McpServerOauthLoginCompletedNotification;
use codex_app_server_protocol::McpServerStatus;

const MCP_MANAGER_VIEW_ID: &str = "mcp-manager";
const MCP_SERVER_ACTIONS_VIEW_ID: &str = "mcp-server-actions";

impl ChatWidget {
    pub(crate) fn open_mcp_manager(&mut self) {
        let params = mcp_loading_params();
        if !self
            .bottom_pane
            .replace_selection_view_if_active(MCP_MANAGER_VIEW_ID, params)
        {
            self.bottom_pane.show_selection_view(mcp_loading_params());
        }
        self.app_event_tx.send(AppEvent::FetchMcpInventory {
            detail: McpServerStatusDetail::Full,
            thread_id: self.thread_id(),
            presentation: McpInventoryPresentation::Manager,
        });
        self.request_redraw();
    }

    pub(crate) fn on_mcp_manager_loaded(&mut self, result: Result<Vec<McpServerStatus>, String>) {
        let params = match result {
            Ok(statuses) => self.mcp_manager_params(statuses),
            Err(err) => mcp_error_params(&err),
        };
        let _ = self
            .bottom_pane
            .replace_selection_view_if_active(MCP_MANAGER_VIEW_ID, params);
        self.request_redraw();
    }

    fn mcp_manager_params(&self, mut statuses: Vec<McpServerStatus>) -> SelectionViewParams {
        statuses.sort_by(|a, b| a.name.cmp(&b.name));
        let mut header = ColumnRenderable::new();
        header.push(Line::from("MCP servers".bold()));
        header.push(Line::from(
            "Select a server to inspect tools or manage authentication.".dim(),
        ));

        let items = if statuses.is_empty() {
            vec![SelectionItem {
                name: "No MCP servers configured".to_string(),
                description: Some("Add a server to config.toml, then reopen /mcp.".to_string()),
                is_disabled: true,
                ..Default::default()
            }]
        } else {
            statuses
                .into_iter()
                .map(|status| {
                    let description = format!(
                        "{} · {} tool{}",
                        auth_status_label(status.auth_status),
                        status.tools.len(),
                        if status.tools.len() == 1 { "" } else { "s" }
                    );
                    let search_value = status
                        .server_info
                        .as_ref()
                        .and_then(|info| info.title.as_ref())
                        .map_or_else(
                            || status.name.clone(),
                            |title| format!("{} {title}", status.name),
                        );
                    let selected_status = status.clone();
                    SelectionItem {
                        name: status.name,
                        description: Some(description.clone()),
                        selected_description: Some(format!(
                            "{description}. Press Enter for tools, details, and authentication options."
                        )),
                        search_value: Some(search_value),
                        actions: vec![Box::new(move |tx| {
                            tx.send(AppEvent::OpenMcpServerActions {
                                status: selected_status.clone(),
                            });
                        })],
                        ..Default::default()
                    }
                })
                .collect()
        };

        SelectionViewParams {
            view_id: Some(MCP_MANAGER_VIEW_ID),
            header: Box::new(header),
            footer_hint: Some(self.bottom_pane.standard_popup_hint_line()),
            items,
            is_searchable: true,
            search_placeholder: Some("Type to search MCP servers".to_string()),
            col_width_mode: ColumnWidthMode::AutoAllRows,
            ..Default::default()
        }
    }

    pub(crate) fn open_mcp_server_actions(&mut self, status: McpServerStatus) {
        let mut header = ColumnRenderable::new();
        header.push(Line::from("MCP server".bold()));
        header.push(Line::from(status.name.clone().bold()));
        if let Some(info) = &status.server_info {
            let title = info.title.as_deref().unwrap_or(&info.name);
            header.push(Line::from(format!("{title} · v{}", info.version).dim()));
        }
        header.push(Line::from(
            format!("Authentication: {}", auth_status_label(status.auth_status)).dim(),
        ));

        let mut items = Vec::new();
        match status.auth_status {
            McpAuthStatus::NotLoggedIn => {
                items.push(self.mcp_oauth_action(&status.name, "Authenticate with OAuth"));
            }
            McpAuthStatus::OAuth => {
                items.push(self.mcp_oauth_action(&status.name, "Reauthenticate with OAuth"));
            }
            McpAuthStatus::BearerToken => items.push(SelectionItem {
                name: "Bearer token authentication".to_string(),
                description: Some(
                    "Configured through the server's bearer_token_env_var environment variable."
                        .to_string(),
                ),
                is_disabled: true,
                ..Default::default()
            }),
            McpAuthStatus::Unsupported => items.push(SelectionItem {
                name: "OAuth unavailable".to_string(),
                description: Some(
                    "This server does not advertise OAuth authentication.".to_string(),
                ),
                is_disabled: true,
                ..Default::default()
            }),
        }

        let status_for_details = status.clone();
        items.push(SelectionItem {
            name: "View tools and resources".to_string(),
            description: Some(format!(
                "{} tools · {} resources · {} templates",
                status.tools.len(),
                status.resources.len(),
                status.resource_templates.len()
            )),
            selected_description: Some(
                "Print the detailed inventory in conversation history.".to_string(),
            ),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::InsertHistoryCell(Box::new(
                    history_cell::new_mcp_tools_output_from_statuses(
                        std::slice::from_ref(&status_for_details),
                        McpServerStatusDetail::Full,
                    ),
                )));
            })],
            dismiss_on_select: true,
            dismiss_parent_on_child_accept: true,
            ..Default::default()
        });

        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(MCP_SERVER_ACTIONS_VIEW_ID),
            header: Box::new(header),
            footer_hint: Some(self.bottom_pane.standard_popup_hint_line()),
            items,
            col_width_mode: ColumnWidthMode::AutoAllRows,
            ..Default::default()
        });
        self.request_redraw();
    }

    fn mcp_oauth_action(&self, name: &str, label: &str) -> SelectionItem {
        let server_name = name.to_string();
        let thread_id = self.thread_id();
        SelectionItem {
            name: label.to_string(),
            description: Some("Open browser-based sign-in for this MCP server.".to_string()),
            selected_description: Some(
                "Codex will open your browser and report when authentication finishes.".to_string(),
            ),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::StartMcpOauthLogin {
                    name: server_name.clone(),
                    thread_id,
                });
            })],
            dismiss_on_select: true,
            dismiss_parent_on_child_accept: true,
            ..Default::default()
        }
    }

    pub(crate) fn on_mcp_oauth_login_completed(
        &mut self,
        notification: McpServerOauthLoginCompletedNotification,
    ) {
        if notification.thread_id.as_deref().is_some_and(|thread_id| {
            self.thread_id()
                .is_some_and(|current_thread_id| current_thread_id.to_string() != thread_id)
        }) {
            return;
        }

        if notification.success {
            self.add_info_message(
                format!("Authenticated MCP server '{}'.", notification.name),
                Some("Run /mcp to review its updated status and tools.".to_string()),
            );
        } else {
            let error = notification
                .error
                .unwrap_or_else(|| "authentication did not complete".to_string());
            self.add_error_message(format!(
                "Failed to authenticate MCP server '{}': {error}",
                notification.name
            ));
        }
    }
}

fn mcp_loading_params() -> SelectionViewParams {
    let mut header = ColumnRenderable::new();
    header.push(Line::from("MCP servers".bold()));
    header.push(Line::from(
        "Loading configured servers and authentication...".dim(),
    ));
    SelectionViewParams {
        view_id: Some(MCP_MANAGER_VIEW_ID),
        header: Box::new(header),
        items: vec![SelectionItem {
            name: "Loading MCP servers...".to_string(),
            description: Some("This updates when the server list is ready.".to_string()),
            is_disabled: true,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn mcp_error_params(error: &str) -> SelectionViewParams {
    let mut header = ColumnRenderable::new();
    header.push(Line::from("MCP servers".bold()));
    header.push(Line::from("Could not load MCP configuration.".dim()));
    SelectionViewParams {
        view_id: Some(MCP_MANAGER_VIEW_ID),
        header: Box::new(header),
        items: vec![SelectionItem {
            name: "Failed to load MCP servers".to_string(),
            description: Some(error.to_string()),
            is_disabled: true,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn auth_status_label(status: McpAuthStatus) -> &'static str {
    match status {
        McpAuthStatus::Unsupported => "No authentication",
        McpAuthStatus::NotLoggedIn => "Sign-in required",
        McpAuthStatus::BearerToken => "Bearer token",
        McpAuthStatus::OAuth => "OAuth connected",
    }
}
