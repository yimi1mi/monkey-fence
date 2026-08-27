#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimarySurface {
    Code,
    Work,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeftPanel {
    Explorer,
    Vcs,
    Workspaces,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BottomPanel {
    Terminal,
    Search,
    Steps,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NavigationState {
    pub surface: PrimarySurface,
    pub left: Option<LeftPanel>,
    pub bottom: Option<BottomPanel>,
    pub agent_open: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavAction {
    ShowCode,
    ShowExplorer,
    ShowVcs,
    ShowWorkspaces,
    ToggleLeft,
    ToggleTerminal,
    ShowSearch,
    ShowSteps,
    ToggleAgent,
    CloseBottom,
}

impl Default for NavigationState {
    fn default() -> Self {
        Self {
            surface: PrimarySurface::Code,
            left: Some(LeftPanel::Explorer),
            bottom: None,
            agent_open: false,
        }
    }
}

impl NavigationState {
    pub fn apply(&mut self, action: NavAction) {
        match action {
            NavAction::ShowCode => self.surface = PrimarySurface::Code,
            NavAction::ShowExplorer => {
                self.surface = PrimarySurface::Code;
                self.left = Some(LeftPanel::Explorer);
            }
            NavAction::ShowVcs => {
                self.surface = PrimarySurface::Code;
                self.left = Some(LeftPanel::Vcs);
            }
            NavAction::ShowWorkspaces => {
                self.surface = PrimarySurface::Work;
                self.left = Some(LeftPanel::Workspaces);
            }
            NavAction::ToggleLeft => {
                self.left = match self.left {
                    Some(_) => None,
                    None => Some(match self.surface {
                        PrimarySurface::Code => LeftPanel::Explorer,
                        PrimarySurface::Work => LeftPanel::Workspaces,
                    }),
                };
            }
            NavAction::ToggleTerminal => {
                self.bottom = if self.bottom == Some(BottomPanel::Terminal) {
                    None
                } else {
                    Some(BottomPanel::Terminal)
                };
            }
            NavAction::ShowSearch => self.bottom = Some(BottomPanel::Search),
            NavAction::ShowSteps => self.bottom = Some(BottomPanel::Steps),
            NavAction::ToggleAgent => self.agent_open = !self.agent_open,
            NavAction::CloseBottom => self.bottom = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_a_quiet_code_workspace() {
        assert_eq!(
            NavigationState::default(),
            NavigationState {
                surface: PrimarySurface::Code,
                left: Some(LeftPanel::Explorer),
                bottom: None,
                agent_open: false,
            }
        );
    }

    #[test]
    fn dock_actions_preserve_the_primary_surface() {
        let mut nav = NavigationState::default();
        nav.apply(NavAction::ShowWorkspaces);

        for action in [
            NavAction::ToggleTerminal,
            NavAction::ShowSearch,
            NavAction::ShowSteps,
            NavAction::ToggleAgent,
            NavAction::CloseBottom,
        ] {
            nav.apply(action);
            assert_eq!(nav.surface, PrimarySurface::Work, "action: {action:?}");
        }
    }

    #[test]
    fn primary_navigation_selects_a_matching_left_panel() {
        let mut nav = NavigationState::default();

        nav.apply(NavAction::ShowWorkspaces);
        assert_eq!(nav.surface, PrimarySurface::Work);
        assert_eq!(nav.left, Some(LeftPanel::Workspaces));

        nav.apply(NavAction::ShowVcs);
        assert_eq!(nav.surface, PrimarySurface::Code);
        assert_eq!(nav.left, Some(LeftPanel::Vcs));
    }

    #[test]
    fn toggles_close_only_their_own_dock() {
        let mut nav = NavigationState::default();
        nav.apply(NavAction::ToggleAgent);
        nav.apply(NavAction::ToggleTerminal);
        nav.apply(NavAction::ToggleTerminal);

        assert!(nav.agent_open);
        assert_eq!(nav.bottom, None);
        assert_eq!(nav.left, Some(LeftPanel::Explorer));
    }
}
