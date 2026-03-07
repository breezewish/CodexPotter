/// Controls how much interim transcript detail is shown in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verbosity {
    /// Most compact view: dims commentary and suppresses low-signal tool chatter.
    #[default]
    Minimal,
    /// Friendly default view: shows interim items without aggressive suppression.
    Simple,
}

impl Verbosity {
    pub fn label(self) -> &'static str {
        match self {
            Verbosity::Minimal => "Minimal",
            Verbosity::Simple => "Simple",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Verbosity::Minimal => "Show results only",
            Verbosity::Simple => "Show key progress updates",
        }
    }

    pub fn config_value(self) -> &'static str {
        match self {
            Verbosity::Minimal => "minimal",
            Verbosity::Simple => "simple",
        }
    }

    pub fn parse_config_value(value: &str) -> Option<Self> {
        match value.trim() {
            "minimal" => Some(Verbosity::Minimal),
            "simple" => Some(Verbosity::Simple),
            // Back-compat with earlier naming experiments.
            "default" | "normal" => Some(Verbosity::Simple),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_config_value_with_back_compat() {
        assert_eq!(
            Verbosity::parse_config_value("minimal"),
            Some(Verbosity::Minimal)
        );
        assert_eq!(
            Verbosity::parse_config_value("simple"),
            Some(Verbosity::Simple)
        );
        assert_eq!(
            Verbosity::parse_config_value("default"),
            Some(Verbosity::Simple)
        );
        assert_eq!(
            Verbosity::parse_config_value("normal"),
            Some(Verbosity::Simple)
        );
        assert_eq!(Verbosity::parse_config_value(""), None);
    }
}
