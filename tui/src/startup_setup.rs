/// Shared startup setup step metadata used by onboarding prompts.
///
/// This is currently used to render "Setup X/Y" markers so users understand how many prompts
/// remain during first-run onboarding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartupSetupStep {
    /// 1-based index of the current step.
    pub index: usize,
    /// Total number of setup steps shown in this startup session.
    pub total: usize,
}

impl StartupSetupStep {
    /// Create a new setup step marker.
    ///
    /// `index` is 1-based and must be <= `total`.
    pub fn new(index: usize, total: usize) -> Self {
        debug_assert!(index > 0, "setup step index must be 1-based");
        debug_assert!(total > 0, "setup step total must be > 0");
        debug_assert!(index <= total, "setup step index must be <= total");
        Self { index, total }
    }

    /// Returns `true` when the marker should be rendered.
    ///
    /// `codex-potter` currently only shows setup markers when there are multiple onboarding
    /// prompts, to avoid a noisy `Setup 1/1` header.
    pub fn should_render(self) -> bool {
        self.total > 1
    }

    /// Format the marker label as `Setup X/Y`.
    pub fn label(self) -> String {
        format!("Setup {}/{}", self.index, self.total)
    }
}
