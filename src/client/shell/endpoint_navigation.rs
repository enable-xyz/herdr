use super::*;

impl ClientShellState {
    pub(super) fn active_endpoint_workspace_at(&self, point: (u16, u16)) -> Option<String> {
        self.hits
            .workspaces
            .iter()
            .find(|hit| {
                hit.endpoint_id == self.active_endpoint_id && super::contains(hit.rect, point)
            })
            .map(|hit| hit.workspace_id.clone())
    }
    pub(super) fn sidebar_action_target_at(
        &self,
        point: (u16, u16),
    ) -> Option<ClientSidebarActionTarget> {
        if let Some((_, endpoint_id, pane_id)) = self
            .hits
            .endpoint_agents
            .iter()
            .find(|(rect, _, _)| super::contains(*rect, point))
        {
            let workspace_id = self
                .endpoints
                .iter()
                .find(|endpoint| endpoint.endpoint_id == *endpoint_id)?
                .snapshot
                .as_deref()?
                .panes
                .iter()
                .find(|pane| pane.pane_id == *pane_id)?
                .workspace_id
                .clone();
            return Some(ClientSidebarActionTarget {
                endpoint_id: endpoint_id.clone(),
                workspace_id,
                pane_id: Some(pane_id.clone()),
            });
        }
        if let Some((_, pane_id)) = self
            .hits
            .agents
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
        {
            let workspace_id = self
                .snapshot
                .as_deref()?
                .panes
                .iter()
                .find(|pane| pane.pane_id == *pane_id)?
                .workspace_id
                .clone();
            return Some(ClientSidebarActionTarget {
                endpoint_id: self.active_endpoint_id.clone(),
                workspace_id,
                pane_id: Some(pane_id.clone()),
            });
        }
        self.hits
            .workspaces
            .iter()
            .find(|hit| super::contains(hit.rect, point))
            .map(|hit| ClientSidebarActionTarget {
                endpoint_id: hit.endpoint_id.clone(),
                workspace_id: hit.workspace_id.clone(),
                pane_id: None,
            })
    }

    pub(super) fn invoke_sidebar_workspace_action(
        &mut self,
        target: ClientSidebarActionTarget,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(action_id) = self.config.workspace_open_action.clone() else {
            return false;
        };
        let Some(snapshot) = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == target.endpoint_id)
            .and_then(|endpoint| endpoint.snapshot.as_deref())
        else {
            return false;
        };
        let Some(workspace) = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == target.workspace_id)
        else {
            return false;
        };
        let pane = target
            .pane_id
            .as_ref()
            .and_then(|pane_id| snapshot.panes.iter().find(|pane| pane.pane_id == *pane_id));
        let agent = target.pane_id.as_ref().and_then(|pane_id| {
            snapshot
                .agents
                .iter()
                .find(|agent| agent.pane_id == *pane_id)
        });
        let context = crate::api::schema::PluginInvocationContext {
            workspace_id: Some(workspace.workspace_id.clone()),
            workspace_label: Some(workspace.label.clone()),
            workspace_cwd: Some(workspace.new_workspace_cwd.clone()),
            worktree: None,
            tab_id: pane.map(|pane| pane.tab_id.clone()),
            tab_label: pane.and_then(|pane| {
                snapshot
                    .tabs
                    .iter()
                    .find(|tab| tab.tab_id == pane.tab_id)
                    .map(|tab| tab.label.clone())
            }),
            focused_pane_id: target.pane_id.clone(),
            focused_pane_cwd: pane.and_then(|pane| pane.cwd.clone()),
            focused_pane_agent: agent.and_then(|agent| agent.agent.clone()),
            focused_pane_status: agent.map(|agent| agent.agent_status),
            selected_text: None,
            invocation_source: Some("sidebar".into()),
            correlation_id: None,
            clicked_url: None,
            link_handler_id: None,
        };
        self.push_endpoint_method_for(
            target.endpoint_id,
            crate::api::schema::Method::PluginActionInvoke(
                crate::api::schema::PluginActionInvokeParams {
                    action_id,
                    plugin_id: None,
                    context: Some(context),
                },
            ),
            PendingEndpointKind::Generic,
            outcome,
        )
    }

    pub(super) fn record_sidebar_action_click(
        &mut self,
        target: ClientSidebarActionTarget,
        now: std::time::Instant,
        outcome: &mut ClientShellInput,
    ) {
        if self.config.workspace_open_action.is_none() {
            self.last_sidebar_action_click = None;
            return;
        }
        const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(350);
        let invoke = self
            .last_sidebar_action_click
            .as_ref()
            .is_some_and(|previous| {
                previous.target == target
                    && !previous.invoked
                    && now.saturating_duration_since(previous.at) <= DOUBLE_CLICK
            });
        if invoke {
            self.invoke_sidebar_workspace_action(target.clone(), outcome);
        }
        self.last_sidebar_action_click = Some(ClientSidebarActionClick {
            target,
            at: now,
            invoked: invoke,
        });
    }

    pub(super) fn endpoint_workspace_is_draggable(&self, press: &ClientWorkspacePress) -> bool {
        press.endpoint_id == self.active_endpoint_id
            && self
                .snapshot
                .as_deref()
                .and_then(|snapshot| {
                    snapshot
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.workspace_id == press.workspace_id)
                })
                .is_some_and(|workspace| {
                    !workspace
                        .worktree
                        .as_ref()
                        .is_some_and(|worktree| worktree.is_linked_worktree)
                })
    }

    pub(super) fn finish_endpoint_workspace_press(
        &mut self,
        press: ClientWorkspacePress,
        outcome: &mut ClientShellInput,
    ) {
        if press.endpoint_id == self.active_endpoint_id {
            self.push_endpoint_method(
                crate::api::schema::Method::WorkspaceFocus(crate::api::schema::WorkspaceTarget {
                    workspace_id: press.workspace_id,
                }),
                outcome,
            );
        } else if self.endpoint_is_online(&press.endpoint_id) {
            outcome.actions.push(ClientShellAction::ActivateEndpoint {
                endpoint_id: press.endpoint_id,
                target: Some(ClientEndpointFocusTarget::Workspace(press.workspace_id)),
            });
        } else {
            let label = self.endpoint_label(&press.endpoint_id).to_owned();
            self.receive_endpoint_unavailable(format!("{label} is not ready"));
            outcome.repaint = true;
        }
    }

    pub(super) fn handle_endpoint_machine_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(endpoint_id) = self
            .hits
            .machines
            .iter()
            .find(|hit| super::contains(hit.rect, point))
            .map(|hit| hit.endpoint_id.clone())
        else {
            return false;
        };
        if endpoint_id == self.active_endpoint_id {
            if !self.collapsed_endpoints.remove(&endpoint_id) {
                self.collapsed_endpoints.insert(endpoint_id);
            }
            outcome.repaint = true;
        } else if self.endpoint_is_online(&endpoint_id) {
            outcome.actions.push(ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target: None,
            });
        } else {
            let label = self.endpoint_label(&endpoint_id).to_owned();
            self.receive_endpoint_unavailable(format!("{label} is not ready"));
            outcome.repaint = true;
        }
        true
    }

    pub(super) fn handle_endpoint_agent_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some((endpoint_id, pane_id)) = self
            .hits
            .endpoint_agents
            .iter()
            .find(|(rect, _, _)| super::contains(*rect, point))
            .map(|(_, endpoint_id, pane_id)| (endpoint_id.clone(), pane_id.clone()))
        else {
            return false;
        };
        if !self.endpoint_is_online(&endpoint_id) {
            let label = self.endpoint_label(&endpoint_id).to_owned();
            self.receive_endpoint_unavailable(format!("{label} is reconnecting"));
            outcome.repaint = true;
        } else if endpoint_id == self.active_endpoint_id {
            self.push_endpoint_method(
                crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget { pane_id }),
                outcome,
            );
        } else {
            outcome.actions.push(ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
            });
        }
        true
    }

    pub(super) fn handle_endpoint_navigation(
        &mut self,
        action: crate::input::KeybindAction,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use crate::input::KeybindAction;
        if !self.multi_endpoint_active() {
            return false;
        }
        if matches!(
            action,
            KeybindAction::PreviousWorkspace | KeybindAction::NextWorkspace
        ) {
            let workspaces = self
                .endpoints
                .iter()
                .filter(|endpoint| endpoint.status == ClientEndpointStatus::Online)
                .flat_map(|endpoint| {
                    endpoint
                        .snapshot
                        .as_deref()
                        .map_or_else(Vec::new, |snapshot| {
                            render::workspace_entries(snapshot, &HashSet::new())
                                .into_iter()
                                .filter_map(|entry| {
                                    snapshot.workspaces.get(entry.index).map(|workspace| {
                                        (
                                            endpoint.endpoint_id.clone(),
                                            workspace.workspace_id.clone(),
                                        )
                                    })
                                })
                                .collect()
                        })
                })
                .collect::<Vec<_>>();
            if workspaces.is_empty() {
                return true;
            }
            let focused = self
                .snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.focused_workspace_id.as_deref());
            let current = workspaces.iter().position(|(endpoint_id, workspace_id)| {
                endpoint_id == &self.active_endpoint_id && Some(workspace_id.as_str()) == focused
            });
            let next = match (current, action) {
                (Some(index), KeybindAction::PreviousWorkspace) => {
                    (index + workspaces.len() - 1) % workspaces.len()
                }
                (Some(index), KeybindAction::NextWorkspace) => (index + 1) % workspaces.len(),
                (None, KeybindAction::PreviousWorkspace) => workspaces.len() - 1,
                (None, KeybindAction::NextWorkspace) => 0,
                _ => unreachable!("endpoint workspace navigation"),
            };
            let (endpoint_id, workspace_id) = workspaces[next].clone();
            self.focus_or_activate(
                endpoint_id,
                ClientEndpointFocusTarget::Workspace(workspace_id),
                outcome,
            );
            return true;
        }
        if matches!(
            action,
            KeybindAction::PreviousAgent | KeybindAction::NextAgent | KeybindAction::FocusAgent(_)
        ) {
            let agents = super::aggregate_navigation::online_agent_targets(
                &self.endpoints,
                self.config.agent_panel_sort,
            );
            if agents.is_empty() {
                return true;
            }
            let next = match action {
                KeybindAction::FocusAgent(index) => {
                    if index >= agents.len() {
                        return true;
                    }
                    index
                }
                KeybindAction::PreviousAgent | KeybindAction::NextAgent => {
                    let focused = self
                        .snapshot
                        .as_deref()
                        .and_then(|snapshot| snapshot.focused_pane_id.as_deref());
                    let current = agents.iter().position(|target| {
                        target.endpoint_id == self.active_endpoint_id
                            && Some(target.pane_id.as_str()) == focused
                    });
                    match (current, action) {
                        (Some(index), KeybindAction::PreviousAgent) => {
                            (index + agents.len() - 1) % agents.len()
                        }
                        (Some(index), KeybindAction::NextAgent) => (index + 1) % agents.len(),
                        (None, KeybindAction::PreviousAgent) => agents.len() - 1,
                        _ => 0,
                    }
                }
                _ => unreachable!("endpoint agent navigation"),
            };
            let target = &agents[next];
            self.focus_or_activate(
                target.endpoint_id.clone(),
                ClientEndpointFocusTarget::Pane(target.pane_id.clone()),
                outcome,
            );
            return true;
        }
        false
    }

    pub(super) fn activate_endpoint(
        &mut self,
        endpoint_id: ClientEndpointId,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.endpoint_is_online(&endpoint_id) {
            let label = self.endpoint_label(&endpoint_id).to_owned();
            self.receive_endpoint_unavailable(format!("{label} is not ready"));
            outcome.repaint = true;
            return false;
        }
        if endpoint_id != self.active_endpoint_id {
            outcome.actions.push(ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target: None,
            });
        }
        true
    }

    pub(super) fn focus_or_activate(
        &mut self,
        endpoint_id: ClientEndpointId,
        target: ClientEndpointFocusTarget,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.endpoint_is_online(&endpoint_id) {
            let label = self.endpoint_label(&endpoint_id).to_owned();
            self.receive_endpoint_unavailable(format!("{label} is not ready"));
            outcome.repaint = true;
            return false;
        }
        if endpoint_id == self.active_endpoint_id {
            let method = match target {
                ClientEndpointFocusTarget::Workspace(workspace_id) => {
                    crate::api::schema::Method::WorkspaceFocus(
                        crate::api::schema::WorkspaceTarget { workspace_id },
                    )
                }
                ClientEndpointFocusTarget::Tab(tab_id) => {
                    crate::api::schema::Method::TabFocus(crate::api::schema::TabTarget { tab_id })
                }
                ClientEndpointFocusTarget::Pane(pane_id) => {
                    crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget {
                        pane_id,
                    })
                }
            };
            self.push_endpoint_method(method, outcome);
        } else {
            outcome.actions.push(ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target: Some(target),
            });
        }
        true
    }
}
