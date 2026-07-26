#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FocusNotification {
    pub(crate) pane_id: String,
    pub(crate) workspace_id: Option<String>,
    pub(crate) status: String,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) group: String,
    pub(crate) app_icon: Option<String>,
}
