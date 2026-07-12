//! Interactive MCP server management for the `/mcp` command.

use super::*;
use crate::app_event::McpInventoryPresentation;
use codex_app_server_protocol::McpAuthStatus;
use codex_app_server_protocol::McpServerOauthLoginCompletedNotification;
use codex_app_server_protocol::McpServerStatus;
use std::collections::HashMap;

const MCP_MANAGER_VIEW_ID: &str = "mcp-manager";
const MCP_SERVER_ACTIONS_VIEW_ID: &str = "mcp-server-actions";
const MCP_SERVER_INVENTORY_VIEW_ID: &str = "mcp-server-inventory";

#[derive(Clone)]
enum McpOauthUiState {
    Starting,
    Waiting,
    Verifying,
    Failed(String),
}

#[derive(Default)]
pub(crate) struct McpManagerState {
    inventory: Option<Result<Vec<McpServerStatus>, String>>,
    loading: bool,
    active_server: Option<McpServerStatus>,
    oauth: HashMap<String, McpOauthUiState>,
}

impl ChatWidget {
    pub(crate) fn prefetch_mcp_manager(&mut self) {
        self.request_mcp_manager_inventory(/*force*/ false);
    }

    pub(crate) fn refresh_mcp_manager(&mut self) {
        self.request_mcp_manager_inventory(/*force*/ true);
    }

    fn request_mcp_manager_inventory(&mut self, force: bool) {
        if self.mcp_manager_state.loading || (!force && self.mcp_manager_state.inventory.is_some())
        {
            return;
        }
        self.mcp_manager_state.loading = true;
        self.app_event_tx.send(AppEvent::FetchMcpInventory {
            detail: McpServerStatusDetail::Full,
            thread_id: self.thread_id(),
            presentation: McpInventoryPresentation::Manager,
        });
    }

    pub(crate) fn open_mcp_manager(&mut self) {
        self.prefetch_mcp_manager();
        let params = self
            .mcp_manager_state
            .inventory
            .clone()
            .map(|result| self.mcp_manager_result_params(result))
            .unwrap_or_else(mcp_loading_params);
        if !self
            .bottom_pane
            .replace_selection_view_if_active(MCP_MANAGER_VIEW_ID, params)
        {
            let params = self
                .mcp_manager_state
                .inventory
                .clone()
                .map(|result| self.mcp_manager_result_params(result))
                .unwrap_or_else(mcp_loading_params);
            self.bottom_pane.show_selection_view(params);
        }
        self.request_redraw();
    }

    pub(crate) fn on_mcp_manager_loaded(&mut self, result: Result<Vec<McpServerStatus>, String>) {
        self.mcp_manager_state.loading = false;
        if let Ok(statuses) = &result {
            for status in statuses {
                if matches!(
                    self.mcp_manager_state.oauth.get(&status.name),
                    Some(McpOauthUiState::Verifying)
                ) {
                    if status.auth_status == McpAuthStatus::OAuth {
                        self.mcp_manager_state.oauth.remove(&status.name);
                    } else {
                        self.mcp_manager_state.oauth.insert(
                            status.name.clone(),
                            McpOauthUiState::Failed(
                                "OAuth completed, but the server still reports sign-in required."
                                    .to_string(),
                            ),
                        );
                    }
                }
            }
            if let Some(active) = &self.mcp_manager_state.active_server
                && let Some(updated) = statuses.iter().find(|status| status.name == active.name)
            {
                self.mcp_manager_state.active_server = Some(updated.clone());
            }
        }
        self.mcp_manager_state.inventory = Some(result.clone());

        let params = self.mcp_manager_result_params(result);
        let _ = self
            .bottom_pane
            .replace_selection_view_if_present(MCP_MANAGER_VIEW_ID, params);
        self.refresh_active_mcp_server_actions();
        self.request_redraw();
    }

    fn mcp_manager_result_params(
        &self,
        result: Result<Vec<McpServerStatus>, String>,
    ) -> SelectionViewParams {
        match result {
            Ok(statuses) => self.mcp_manager_params(statuses),
            Err(err) => mcp_error_params(&err),
        }
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
                description: Some("Add a server to config.toml, then restart Codex.".to_string()),
                is_disabled: true,
                ..Default::default()
            }]
        } else {
            statuses
                .into_iter()
                .map(|status| {
                    let auth = self
                        .mcp_manager_state
                        .oauth
                        .get(&status.name)
                        .map(oauth_status_label)
                        .unwrap_or_else(|| auth_status_label(status.auth_status).to_string());
                    let description = format!(
                        "{auth} · {} tool{}",
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
        self.mcp_manager_state.active_server = Some(status.clone());
        self.bottom_pane
            .show_selection_view(self.mcp_server_actions_params(&status));
        self.request_redraw();
    }

    fn refresh_active_mcp_server_actions(&mut self) {
        let Some(status) = self.mcp_manager_state.active_server.clone() else {
            return;
        };
        let params = self.mcp_server_actions_params(&status);
        let _ = self
            .bottom_pane
            .replace_selection_view_if_active(MCP_SERVER_ACTIONS_VIEW_ID, params);
    }

    fn mcp_server_actions_params(&self, status: &McpServerStatus) -> SelectionViewParams {
        let mut header = ColumnRenderable::new();
        header.push(Line::from("MCP server".bold()));
        header.push(Line::from(status.name.clone().bold()));
        if let Some(info) = &status.server_info {
            let title = info.title.as_deref().unwrap_or(&info.name);
            header.push(Line::from(format!("{title} · v{}", info.version).dim()));
        }
        let auth = self
            .mcp_manager_state
            .oauth
            .get(&status.name)
            .map(oauth_status_label)
            .unwrap_or_else(|| auth_status_label(status.auth_status).to_string());
        header.push(Line::from(format!("Authentication: {auth}").dim()));

        let mut items = Vec::new();
        if let Some(state) = self.mcp_manager_state.oauth.get(&status.name) {
            items.push(SelectionItem {
                name: oauth_status_label(state),
                description: oauth_status_detail(state),
                is_disabled: true,
                ..Default::default()
            });
        } else {
            match status.auth_status {
                McpAuthStatus::NotLoggedIn => {
                    items.push(self.mcp_oauth_action(&status.name, "Authenticate with OAuth"));
                }
                McpAuthStatus::OAuth => {
                    items.push(self.mcp_oauth_action(&status.name, "Reauthenticate with OAuth"));
                }
                McpAuthStatus::BearerToken => items.push(disabled_item(
                    "Bearer token authentication",
                    "Configured through bearer_token_env_var.",
                )),
                McpAuthStatus::Unsupported => items.push(disabled_item(
                    "OAuth unavailable",
                    "This server does not advertise OAuth authentication.",
                )),
            }
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
            selected_description: Some("Open the inventory inside the MCP manager.".to_string()),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::OpenMcpServerInventory {
                    status: status_for_details.clone(),
                });
            })],
            ..Default::default()
        });

        SelectionViewParams {
            view_id: Some(MCP_SERVER_ACTIONS_VIEW_ID),
            header: Box::new(header),
            footer_hint: Some(self.bottom_pane.standard_popup_hint_line()),
            items,
            col_width_mode: ColumnWidthMode::AutoAllRows,
            ..Default::default()
        }
    }

    pub(crate) fn open_mcp_server_inventory(&mut self, status: McpServerStatus) {
        let mut items = Vec::new();
        let mut tools = status.tools.into_iter().collect::<Vec<_>>();
        tools.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (name, tool) in tools {
            items.push(disabled_item(
                &format!("Tool  {name}"),
                tool.description.as_deref().unwrap_or("No description"),
            ));
        }
        for resource in status.resources {
            items.push(disabled_item(
                &format!("Resource  {}", resource.name),
                &resource.uri,
            ));
        }
        for template in status.resource_templates {
            items.push(disabled_item(
                &format!("Template  {}", template.name),
                &template.uri_template,
            ));
        }
        if items.is_empty() {
            items.push(disabled_item(
                "No tools or resources available",
                "Authenticate first if this server requires sign-in.",
            ));
        }
        let mut header = ColumnRenderable::new();
        header.push(Line::from(format!("{} inventory", status.name).bold()));
        header.push(Line::from("Tools, resources, and templates".dim()));
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(MCP_SERVER_INVENTORY_VIEW_ID),
            header: Box::new(header),
            footer_hint: Some(self.bottom_pane.standard_popup_hint_line()),
            items,
            is_searchable: true,
            search_placeholder: Some("Search this server's inventory".to_string()),
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
                "This panel will track browser sign-in and verify completion.".to_string(),
            ),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::StartMcpOauthLogin {
                    name: server_name.clone(),
                    thread_id,
                });
            })],
            dismiss_on_select: false,
            dismiss_parent_on_child_accept: false,
            ..Default::default()
        }
    }

    pub(crate) fn on_mcp_oauth_login_starting(&mut self, name: &str) {
        self.mcp_manager_state
            .oauth
            .insert(name.to_string(), McpOauthUiState::Starting);
        self.refresh_active_mcp_server_actions();
        self.request_redraw();
    }

    pub(crate) fn on_mcp_oauth_browser_opened(&mut self, name: &str, result: Result<(), String>) {
        let state = match result {
            Ok(()) => McpOauthUiState::Waiting,
            Err(error) => McpOauthUiState::Failed(error),
        };
        self.mcp_manager_state.oauth.insert(name.to_string(), state);
        self.refresh_active_mcp_server_actions();
        self.request_redraw();
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
        let state = if notification.success {
            McpOauthUiState::Verifying
        } else {
            McpOauthUiState::Failed(
                notification
                    .error
                    .unwrap_or_else(|| "authentication did not complete".to_string()),
            )
        };
        self.mcp_manager_state
            .oauth
            .insert(notification.name, state);
        self.refresh_active_mcp_server_actions();
        if notification.success {
            self.refresh_mcp_manager();
        }
        self.request_redraw();
    }
}

fn disabled_item(name: &str, description: &str) -> SelectionItem {
    SelectionItem {
        name: name.to_string(),
        description: Some(description.to_string()),
        is_disabled: true,
        ..Default::default()
    }
}

fn oauth_status_label(state: &McpOauthUiState) -> String {
    match state {
        McpOauthUiState::Starting => "Starting OAuth…".to_string(),
        McpOauthUiState::Waiting => "Waiting for browser sign-in…".to_string(),
        McpOauthUiState::Verifying => "Sign-in complete · verifying…".to_string(),
        McpOauthUiState::Failed(_) => "OAuth failed".to_string(),
    }
}

fn oauth_status_detail(state: &McpOauthUiState) -> Option<String> {
    match state {
        McpOauthUiState::Starting => Some("Requesting an authorization URL…".to_string()),
        McpOauthUiState::Waiting => {
            Some("Complete sign-in in your browser; this panel updates automatically.".to_string())
        }
        McpOauthUiState::Verifying => {
            Some("Confirming server authentication and tools…".to_string())
        }
        McpOauthUiState::Failed(error) => Some(error.clone()),
    }
}

fn mcp_loading_params() -> SelectionViewParams {
    let mut header = ColumnRenderable::new();
    header.push(Line::from("MCP servers".bold()));
    header.push(Line::from("Finishing the startup MCP scan…".dim()));
    SelectionViewParams {
        view_id: Some(MCP_MANAGER_VIEW_ID),
        header: Box::new(header),
        items: vec![disabled_item(
            "Loading MCP servers…",
            "The startup inventory will appear here automatically.",
        )],
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
        items: vec![disabled_item("Failed to load MCP servers", error)],
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
