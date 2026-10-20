//! The light or dark theme: the system's, or the user's override kept in
//! `localStorage`.

/// The theme the user picked.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThemePreference {
    /// Follow the system setting.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePreference {
    /// The preference a stored value names; the system's for anything else.
    pub fn parse(stored: Option<&str>) -> Self {
        match stored {
            Some("light") => Self::Light,
            Some("dark") => Self::Dark,
            _ => Self::System,
        }
    }

    /// The value kept in `localStorage`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// The preference the theme button switches to next.
    pub const fn next(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }

    /// Whether the page is dark, given whether the system prefers dark.
    pub const fn is_dark(self, system_dark: bool) -> bool {
        match self {
            Self::System => system_dark,
            Self::Light => false,
            Self::Dark => true,
        }
    }

    /// The theme button's label.
    /// The preference's name, as the theme button says it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ThemePreference;

    #[test]
    fn the_preference_round_trips_and_cycles_through_every_choice() {
        for preference in [
            ThemePreference::System,
            ThemePreference::Light,
            ThemePreference::Dark,
        ] {
            assert_eq!(
                ThemePreference::parse(Some(preference.as_str())),
                preference
            );
            assert_eq!(preference.next().next().next(), preference);
        }
        assert_eq!(
            ThemePreference::parse(Some("sepia")),
            ThemePreference::System
        );
        assert_eq!(ThemePreference::parse(None), ThemePreference::System);
    }

    #[test]
    fn only_the_system_preference_follows_the_system() {
        assert!(ThemePreference::System.is_dark(true));
        assert!(!ThemePreference::System.is_dark(false));
        assert!(!ThemePreference::Light.is_dark(true));
        assert!(ThemePreference::Dark.is_dark(false));
    }
}
