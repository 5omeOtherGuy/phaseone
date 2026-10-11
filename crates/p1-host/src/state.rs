//! The host's state directory (ADR-0171): the ONE resolution every durable host state
//! goes through — the session catalog (`session/catalog.rs`) for the CLI's default
//! journals and for `p1 acp`, and the workflow runs that have no session file
//! (ADR-0053 item 7).

use std::path::PathBuf;

use p1_auth::Locations;

/// `$P1_STATE_DIR`, else `$XDG_STATE_HOME/p1`, else `$HOME/.local/state/p1`. A blank
/// variable counts as unset, as `settings_path` reads them. `None` when the host has
/// none of the three. `P1_STATE_DIR` is how a test, or an operator, puts every piece of
/// state somewhere else without moving the rest of `XDG_STATE_HOME`.
pub fn root(locations: &Locations) -> Option<PathBuf> {
    let dir = |name: &str| {
        locations
            .env(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    dir("P1_STATE_DIR")
        .or_else(|| dir("XDG_STATE_HOME").map(|state| state.join("p1")))
        .or_else(|| dir("HOME").map(|home| home.join(".local").join("state").join("p1")))
}

#[cfg(test)]
mod tests {
    use super::root;
    use p1_auth::Locations;
    use std::path::PathBuf;

    fn locations(vars: &[(&str, &str)]) -> Locations {
        Locations::from_environment(vars.iter().map(|(name, value)| (name.into(), value.into())))
    }

    #[test]
    fn p1_state_dir_wins_then_xdg_state_home_then_home() {
        let all = [
            ("P1_STATE_DIR", "/s"),
            ("XDG_STATE_HOME", "/x"),
            ("HOME", "/h"),
        ];
        assert_eq!(root(&locations(&all)), Some(PathBuf::from("/s")));
        assert_eq!(root(&locations(&all[1..])), Some(PathBuf::from("/x/p1")));
        assert_eq!(
            root(&locations(&all[2..])),
            Some(PathBuf::from("/h/.local/state/p1"))
        );
        assert_eq!(root(&locations(&[])), None);
    }

    #[test]
    fn a_blank_variable_counts_as_unset() {
        assert_eq!(
            root(&locations(&[
                ("P1_STATE_DIR", ""),
                ("XDG_STATE_HOME", "/x")
            ])),
            Some(PathBuf::from("/x/p1"))
        );
    }
}
