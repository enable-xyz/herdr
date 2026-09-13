use super::*;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientLaunchOptions {
    pub workspace_id: Option<String>,
    pub pane_id: Option<String>,
    pub hide_sidebar: bool,
}

pub fn parse_client_launch_args(args: &[String]) -> Result<ClientLaunchOptions, String> {
    let mut options = ClientLaunchOptions::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" => {
                let value = args
                    .get(index + 1)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "missing value for --workspace".to_owned())?;
                if options.workspace_id.replace(value.clone()).is_some() {
                    return Err("--workspace may only be specified once".into());
                }
                index += 2;
            }
            "--pane" => {
                let value = args
                    .get(index + 1)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "missing value for --pane".to_owned())?;
                if options.pane_id.replace(value.clone()).is_some() {
                    return Err("--pane may only be specified once".into());
                }
                index += 2;
            }
            "--hide-sidebar" => {
                if options.hide_sidebar {
                    return Err("--hide-sidebar may only be specified once".into());
                }
                options.hide_sidebar = true;
                index += 1;
            }
            argument => return Err(format!("unknown client option: {argument}")),
        }
    }
    if options.pane_id.is_some() && options.workspace_id.is_none() {
        return Err("--pane requires --workspace".into());
    }
    Ok(options)
}

/// Runs the thin client and enters the main event loop.
pub fn run_client(options: ClientLaunchOptions) -> io::Result<()> {
    run_client_with_mode(None, None, "connecting to server", options)
}

#[cfg(unix)]
pub fn run_terminal_attach(terminal_id: String, takeover: bool) -> io::Result<()> {
    run_client_with_mode(
        Some((terminal_id, takeover)),
        Some(AttachEscapeState::default()),
        "attaching to terminal",
        ClientLaunchOptions::default(),
    )
}

#[cfg(windows)]
pub fn run_terminal_attach(_terminal_id: String, _takeover: bool) -> io::Result<()> {
    debug_assert!(!crate::platform::capabilities().direct_terminal_attach);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "direct terminal attach is not supported on Windows yet",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_launch_parser_accepts_exact_target_and_sidebar_override() {
        let args = [
            "--workspace",
            "workspace-1",
            "--pane",
            "pane-2",
            "--hide-sidebar",
        ]
        .map(str::to_owned);
        assert_eq!(
            parse_client_launch_args(&args).unwrap(),
            ClientLaunchOptions {
                workspace_id: Some("workspace-1".into()),
                pane_id: Some("pane-2".into()),
                hide_sidebar: true,
            }
        );
    }

    #[test]
    fn client_launch_parser_requires_workspace_for_pane() {
        let args = ["--pane", "pane-2"].map(str::to_owned);
        assert_eq!(
            parse_client_launch_args(&args).unwrap_err(),
            "--pane requires --workspace"
        );
    }
}
