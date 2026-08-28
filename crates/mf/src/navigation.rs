//! 纯导航状态机(多项目工作台)。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimarySurface {
    Code,
    /// Agent Workspace:Agents 看板 / Pipeline 视图
    Work,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyState {
    FirstLaunch,
    ProjectReady,
}

pub fn empty_state_for(has_project: bool, has_tabs: bool) -> Option<EmptyState> {
    if has_tabs {
        None
    } else if has_project {
        Some(EmptyState::ProjectReady)
    } else {
        Some(EmptyState::FirstLaunch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftPanel {
    Explorer,
    Tasks,
    Vcs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BottomPanel {
    Terminal,
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavigationState {
    pub surface: PrimarySurface,
    pub left: Option<LeftPanel>,
    pub bottom: Option<BottomPanel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavAction {
    ShowCode,
    ShowWork,
    ShowExplorer,
    ShowVcs,
    ShowTasks,
    ToggleLeft,
    ToggleTerminal,
    ShowSearch,
    CloseBottom,
}

impl Default for NavigationState {
    fn default() -> Self {
        Self {
            surface: PrimarySurface::Code,
            left: Some(LeftPanel::Explorer),
            bottom: None,
        }
    }
}

impl NavigationState {
    pub fn apply(&mut self, action: NavAction) {
        match action {
            NavAction::ShowCode => self.surface = PrimarySurface::Code,
            NavAction::ShowWork => self.surface = PrimarySurface::Work,
            NavAction::ShowExplorer => {
                self.surface = PrimarySurface::Code;
                self.left = Some(LeftPanel::Explorer);
            }
            NavAction::ShowVcs => {
                self.surface = PrimarySurface::Code;
                self.left = Some(LeftPanel::Vcs);
            }
            NavAction::ShowTasks => {
                self.surface = PrimarySurface::Work;
                self.left = Some(LeftPanel::Tasks);
            }
            NavAction::ToggleLeft => {
                self.left = match self.left {
                    Some(_) => None,
                    None => Some(match self.surface {
                        PrimarySurface::Code => LeftPanel::Explorer,
                        PrimarySurface::Work => LeftPanel::Tasks,
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
            }
        );
    }

    #[test]
    fn dock_actions_preserve_the_primary_surface() {
        let mut nav = NavigationState::default();
        nav.apply(NavAction::ShowTasks);

        for action in [
            NavAction::ToggleTerminal,
            NavAction::ShowSearch,
            NavAction::CloseBottom,
        ] {
            nav.apply(action);
            assert_eq!(nav.surface, PrimarySurface::Work, "action: {action:?}");
        }
    }

    #[test]
    fn primary_navigation_selects_a_matching_left_panel() {
        let mut nav = NavigationState::default();

        nav.apply(NavAction::ShowTasks);
        assert_eq!(nav.surface, PrimarySurface::Work);
        assert_eq!(nav.left, Some(LeftPanel::Tasks));

        nav.apply(NavAction::ShowVcs);
        assert_eq!(nav.surface, PrimarySurface::Code);
        assert_eq!(nav.left, Some(LeftPanel::Vcs));
    }

    #[test]
    fn toggles_close_only_their_own_dock() {
        let mut nav = NavigationState::default();
        nav.apply(NavAction::ToggleTerminal);
        nav.apply(NavAction::ToggleTerminal);
        assert_eq!(nav.bottom, None);
        assert_eq!(nav.left, Some(LeftPanel::Explorer));
    }

    #[test]
    fn opened_project_without_tabs_uses_project_ready_state() {
        assert_eq!(empty_state_for(true, false), Some(EmptyState::ProjectReady));
    }

    #[test]
    fn first_launch_and_open_editor_are_distinct() {
        assert_eq!(empty_state_for(false, false), Some(EmptyState::FirstLaunch));
        assert_eq!(empty_state_for(true, true), None);
    }
}
