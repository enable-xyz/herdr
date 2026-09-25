use std::time::Instant;

use bytes::Bytes;
use ratatui::layout::Rect;

use super::App;

struct PendingAgentResumeCandidate {
    pane_id: crate::layout::PaneId,
    terminal_id: crate::terminal::TerminalId,
    cwd: std::path::PathBuf,
    plan: crate::agent_resume::AgentResumePlan,
    rows: u16,
    cols: u16,
}

impl App {
    pub(crate) fn has_pending_agent_resumes(&self) -> bool {
        self.state
            .terminals
            .values()
            .any(|terminal| terminal.pending_agent_resume_plan.is_some())
    }

    pub(crate) fn sync_pending_agent_resume_deadline(
        &mut self,
        now: Instant,
        surfaces: &[(crate::ui::TabSurfaceTarget, Rect)],
    ) {
        if self.pending_agent_resume_candidates(surfaces).is_empty() {
            self.pending_agent_resume_deadline = None;
            return;
        }
        self.pending_agent_resume_deadline
            .get_or_insert(now + super::PENDING_AGENT_RESUME_THEME_WAIT);
    }

    pub(crate) fn pending_agent_resume_due(&self, now: Instant) -> bool {
        self.pending_agent_resume_deadline
            .is_some_and(|deadline| now >= deadline)
    }

    pub(crate) fn start_pending_agent_resumes(
        &mut self,
        surfaces: &[(crate::ui::TabSurfaceTarget, Rect)],
        allow_empty_theme: bool,
    ) -> bool {
        let pending = self.pending_agent_resume_candidates(surfaces);
        let mut changed = false;
        for PendingAgentResumeCandidate {
            pane_id,
            terminal_id,
            cwd,
            plan,
            rows,
            cols,
        } in pending
        {
            if self.terminal_runtimes.get(&terminal_id).is_some() {
                continue;
            }
            changed |= self.start_pending_agent_resume(
                pane_id,
                terminal_id,
                cwd,
                plan,
                rows,
                cols,
                allow_empty_theme,
            );
        }

        if changed {
            self.schedule_session_save();
        }
        if self.pending_agent_resume_candidates(surfaces).is_empty() {
            self.pending_agent_resume_deadline = None;
        } else if allow_empty_theme {
            // A failed launch remains pending; do not spin on the elapsed theme deadline.
            self.pending_agent_resume_deadline =
                Some(Instant::now() + super::PENDING_AGENT_RESUME_THEME_WAIT);
        }
        changed
    }

    fn pending_agent_resume_candidates(
        &self,
        surfaces: &[(crate::ui::TabSurfaceTarget, Rect)],
    ) -> Vec<PendingAgentResumeCandidate> {
        if !self.has_pending_agent_resumes() {
            return Vec::new();
        }
        let mut pending = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for &(target, area) in surfaces {
            if area.width == 0 || area.height == 0 {
                continue;
            }
            let Some(tab) = self
                .state
                .workspaces
                .get(target.workspace_index)
                .and_then(|workspace| workspace.tabs.get(target.tab_index))
            else {
                continue;
            };
            let layout = crate::ui::compute_tab_surface_for(
                &self.state,
                &self.terminal_runtimes,
                Some(target),
                area,
                false,
                crate::kitty_graphics::HostCellSize::default(),
            );
            for info in layout.pane_infos {
                if info.inner_rect.width == 0 || info.inner_rect.height == 0 {
                    continue;
                }
                let Some(pane) = tab.panes.get(&info.id) else {
                    continue;
                };
                if !seen.insert(pane.attached_terminal_id.clone())
                    || self
                        .terminal_runtimes
                        .get(&pane.attached_terminal_id)
                        .is_some()
                {
                    continue;
                }
                let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id) else {
                    continue;
                };
                let Some(plan) = terminal.pending_agent_resume_plan.clone() else {
                    continue;
                };
                pending.push(PendingAgentResumeCandidate {
                    pane_id: info.id,
                    terminal_id: pane.attached_terminal_id.clone(),
                    cwd: terminal.cwd.clone(),
                    plan,
                    rows: info.inner_rect.height,
                    cols: info.inner_rect.width,
                });
            }
        }
        pending
    }

    pub(crate) fn start_pending_agent_resume_for_terminal(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        rows: u16,
        cols: u16,
        allow_empty_theme: bool,
    ) -> bool {
        if self.terminal_runtimes.get(terminal_id).is_some() {
            return false;
        }
        let Some((pane_id, cwd, plan)) = self.state.workspaces.iter().find_map(|ws| {
            ws.tabs.iter().find_map(|tab| {
                tab.layout.pane_ids().into_iter().find_map(|pane_id| {
                    let pane = tab.panes.get(&pane_id)?;
                    if &pane.attached_terminal_id != terminal_id {
                        return None;
                    }
                    let terminal = self.state.terminals.get(terminal_id)?;
                    Some((
                        pane_id,
                        terminal.cwd.clone(),
                        terminal.pending_agent_resume_plan.clone()?,
                    ))
                })
            })
        }) else {
            return false;
        };

        let changed = self.start_pending_agent_resume(
            pane_id,
            terminal_id.clone(),
            cwd,
            plan,
            rows,
            cols,
            allow_empty_theme,
        );
        if changed {
            self.schedule_session_save();
        }
        if !self.has_pending_agent_resumes() {
            self.pending_agent_resume_deadline = None;
        }
        changed
    }

    fn start_pending_agent_resume(
        &mut self,
        pane_id: crate::layout::PaneId,
        terminal_id: crate::terminal::TerminalId,
        cwd: std::path::PathBuf,
        plan: crate::agent_resume::AgentResumePlan,
        rows: u16,
        cols: u16,
        allow_empty_theme: bool,
    ) -> bool {
        let host_terminal_theme = self.state.host_terminal_theme;
        if host_terminal_theme.is_empty() && !allow_empty_theme {
            return false;
        }

        let Some(resume_command) = shell_command_from_argv(&plan.argv) else {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                "failed to start deferred agent resume with empty argv"
            );
            return false;
        };
        let Some(launch_env) = self
            .find_pane(pane_id)
            .and_then(|(ws_idx, _)| self.pane_launch_env(ws_idx, pane_id, Vec::new()))
        else {
            return false;
        };

        let runtime = match crate::terminal::TerminalRuntime::spawn(
            pane_id,
            rows,
            cols,
            cwd,
            self.state.pane_scrollback_limit_bytes,
            host_terminal_theme,
            self.state.host_terminal_appearance,
            crate::pane::PaneShellConfig::new(&self.state.default_shell, self.state.shell_mode),
            &launch_env,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::warn!(
                    pane = pane_id.raw(),
                    terminal = %terminal_id,
                    agent = %plan.agent,
                    err = %err,
                    "failed to start shell for deferred agent resume"
                );
                return false;
            }
        };

        let mut input = resume_command;
        input.push('\r');
        if let Err(err) = runtime.try_send_bytes(Bytes::from(input)) {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                err = %err,
                "failed to send deferred agent resume command to shell"
            );
            runtime.shutdown();
            return false;
        }

        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.pending_agent_resume_plan = None;
            terminal.respawn_shell_on_exit = false;
        }
        true
    }
}

fn shell_command_from_argv(argv: &[String]) -> Option<String> {
    let mut parts = argv.iter();
    let first = shell_quote(parts.next()?);
    let mut command = first;
    for part in parts {
        command.push(' ');
        command.push_str(&shell_quote(part));
    }
    Some(command)
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b'='
            )
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    #[cfg(unix)]
    fn long_running_test_argv() -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()]
    }

    #[cfg(unix)]
    fn marker_resume_test_argv() -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s' 'restored agent: shell quoted | marker'; sleep 5".into(),
        ]
    }
    #[cfg(unix)]
    fn visible_surface(
        workspace_index: usize,
        tab_index: usize,
    ) -> Vec<(crate::ui::TabSurfaceTarget, Rect)> {
        vec![(
            crate::ui::TabSurfaceTarget {
                workspace_index,
                tab_index,
            },
            Rect::new(0, 0, 100, 30),
        )]
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_waits_for_host_theme_before_launch() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.view.pane_infos = pane_infos;
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: marker_resume_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        assert!(!app.start_pending_agent_resumes(&visible_surface(0, 0), false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());

        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };

        assert!(app.start_pending_agent_resumes(&visible_surface(0, 0), false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());
        let terminal = app
            .state
            .terminals
            .get(&terminal_id)
            .expect("terminal should survive launch");
        assert!(terminal.pending_agent_resume_plan.is_none());
        assert!(!terminal.respawn_shell_on_exit);

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_can_launch_after_theme_wait_expires() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        let surfaces = visible_surface(0, 0);
        app.sync_pending_agent_resume_deadline(std::time::Instant::now(), &surfaces);
        assert!(!app.start_pending_agent_resumes(&surfaces, false));
        assert!(app.start_pending_agent_resumes(&surfaces, true));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restored_panes_launch_only_when_exposed_across_workspace_and_tab_surfaces() {
        let mut app = test_app();
        let mut first = crate::workspace::Workspace::test_new("first");
        let first_terminal = first.terminal_id(first.tabs[0].root_pane).unwrap().clone();
        let second_tab = first.test_add_tab(Some("second"));
        let second_terminal = first.tabs[second_tab]
            .terminal_id(first.tabs[second_tab].root_pane)
            .unwrap()
            .clone();
        let hidden = crate::workspace::Workspace::test_new("hidden");
        let hidden_terminal = hidden
            .terminal_id(hidden.tabs[0].root_pane)
            .unwrap()
            .clone();
        app.state.workspaces = vec![first, hidden];
        for i in 0..126 {
            app.state
                .workspaces
                .push(crate::workspace::Workspace::test_new(&format!("saved-{i}")));
        }
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        assert_eq!(app.state.terminals.len(), 129);
        for (index, (terminal_id, terminal)) in app.state.terminals.iter_mut().enumerate() {
            terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                agent: "codex".into(),
                argv: long_running_test_argv(),
                dedupe_key: format!("session:{terminal_id}"),
            });
            terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "herdr:codex".into(),
                agent: "codex".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id(format!("saved-{index}"))
                    .unwrap(),
            });
        }
        app.sync_pending_agent_resume_deadline(Instant::now(), &[]);
        assert!(!app.start_pending_agent_resumes(&[], true));
        assert!(app.pending_agent_resume_deadline.is_none());
        assert_eq!(
            app.state
                .terminals
                .values()
                .filter(|t| t.pending_agent_resume_plan.is_some())
                .count(),
            129
        );

        let first_surface = visible_surface(0, 0);
        app.state.pane_scrollbars = true;
        let initial_cols = app.pending_agent_resume_candidates(&first_surface)[0].cols;
        assert!(app.start_pending_agent_resumes(&first_surface, true));
        assert!(!app.start_pending_agent_resumes(&first_surface, true));
        assert!(app.terminal_runtimes.get(&first_terminal).is_some());
        let rendered = crate::ui::compute_tab_surface_for(
            &app.state,
            &app.terminal_runtimes,
            Some(first_surface[0].0),
            first_surface[0].1,
            false,
            crate::kitty_graphics::HostCellSize::default(),
        );
        assert_eq!(
            initial_cols, rendered.pane_infos[0].inner_rect.width,
            "a deferred terminal must start at its rendered viewport width",
        );
        assert!(app.terminal_runtimes.get(&second_terminal).is_none());
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_none());
        assert_eq!(
            app.state
                .terminals
                .values()
                .filter(|t| t.pending_agent_resume_plan.is_some())
                .count(),
            128
        );
        let snapshot = crate::persist::capture(
            &app.state.workspaces,
            &app.state.terminals,
            &app.terminal_runtimes,
            app.state.active,
            app.state.selected,
        );
        assert_eq!(
            snapshot
                .workspaces
                .iter()
                .flat_map(|ws| &ws.tabs)
                .flat_map(|tab| tab.panes.values())
                .filter(|pane| pane.agent_session.is_some())
                .count(),
            129,
        );

        let both = [first_surface, visible_surface(0, second_tab)].concat();
        assert!(app.start_pending_agent_resumes(&both, true));
        assert!(app.terminal_runtimes.get(&second_terminal).is_some());
        assert!(app.state.terminals[&hidden_terminal]
            .pending_agent_resume_plan
            .is_some());
        assert!(app.start_pending_agent_resumes(&visible_surface(1, 0), true));
        assert_eq!(app.terminal_runtimes.len(), 3);
        assert_eq!(
            app.state
                .terminals
                .values()
                .filter(|t| t.pending_agent_resume_plan.is_some())
                .count(),
            126
        );
        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_terminal_attach_starts_an_unexposed_resume_once() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("hidden");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "direct-terminal".into(),
        });
        assert!(app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true));
        assert!(!app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true));
        assert_eq!(
            app.terminal_runtimes
                .get(&terminal_id)
                .unwrap()
                .current_size(),
            (24, 80)
        );
        assert!(app.state.terminals[&terminal_id]
            .pending_agent_resume_plan
            .is_none());
        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_visible_resume_remains_retryable() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("retry");
        let terminal_id = workspace
            .terminal_id(workspace.tabs[0].root_pane)
            .unwrap()
            .clone();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec![],
            dedupe_key: "retry".into(),
        });
        assert!(!app.start_pending_agent_resumes(&visible_surface(0, 0), true));
        assert!(app.state.terminals[&terminal_id]
            .pending_agent_resume_plan
            .is_some());
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .pending_agent_resume_plan
            .as_mut()
            .unwrap()
            .argv = long_running_test_argv();
        assert!(app.start_pending_agent_resumes(&visible_surface(0, 0), true));
        assert!(app.state.terminals[&terminal_id]
            .pending_agent_resume_plan
            .is_none());
        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn zoom_hidden_restored_pane_keeps_plan_until_unzoomed() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("zoomed");
        let hidden_pane = workspace.tabs[0].root_pane;
        workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.tabs[0].zoomed = true;
        let hidden_terminal = workspace.terminal_id(hidden_pane).unwrap().clone();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&hidden_terminal)
            .unwrap()
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "zoom-hidden".into(),
        });
        assert!(!app.start_pending_agent_resumes(&visible_surface(0, 0), true));
        assert!(app.state.terminals[&hidden_terminal]
            .pending_agent_resume_plan
            .is_some());
        app.state.workspaces[0].tabs[0].zoomed = false;
        assert!(app.start_pending_agent_resumes(&visible_surface(0, 0), true));
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_launches_with_inner_rect_size() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("split");
        let pane_id = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 100, 30),
            inner_rect: ratatui::layout::Rect::new(1, 1, 98, 28),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::ALL,
            is_focused: true,
        }];
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        let expected = crate::ui::compute_tab_surface_for(
            &app.state,
            &app.terminal_runtimes,
            Some(crate::ui::TabSurfaceTarget {
                workspace_index: 0,
                tab_index: 0,
            }),
            Rect::new(0, 0, 100, 30),
            false,
            crate::kitty_graphics::HostCellSize::default(),
        )
        .pane_infos
        .into_iter()
        .find(|info| info.id == pane_id)
        .unwrap()
        .inner_rect;
        assert!(app.start_pending_agent_resumes(&visible_surface(0, 0), false));
        assert_eq!(
            app.terminal_runtimes
                .get(&terminal_id)
                .unwrap()
                .current_size(),
            (expected.height, expected.width)
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[test]
    fn shell_command_from_argv_quotes_resume_arguments() {
        let argv = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "session with ' quote".to_string(),
        ];

        assert_eq!(
            shell_command_from_argv(&argv).as_deref(),
            Some("claude --resume 'session with '\\'' quote'")
        );
        assert_eq!(shell_command_from_argv(&[]), None);
    }
}
