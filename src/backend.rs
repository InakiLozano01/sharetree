//! Which backend `sharetree add` uses (research note §7.2). Pure, so the
//! policy is unit-tested apart from the filesystem probing that feeds it.
//!
//! 1. A real reflink probe beside the destination. Success means the
//!    existing reflink engine runs there. If the destination is inside a
//!    sharetree volume, the volume's seed checkout is made the donor.
//! 2. The probe failed and the repository has a configured volume (Linux
//!    loop image, Windows Dev Drive): with `--into-volume` or
//!    `auto_volume = true` the worktree is placed inside the volume; with
//!    `auto_volume = false` plain git. When the user has not chosen
//!    (`auto_volume` unset for the repository and globally), `Ask`: the
//!    caller prompts on a terminal (`Answer`) and otherwise uses plain git
//!    with a hint on how to choose.
//! 3. Nothing configured: plain git, with a hint pointing to `sharetree setup`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
    Windows,
    Other,
}

impl Platform {
    pub fn current() -> Platform {
        if cfg!(target_os = "linux") {
            Platform::Linux
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Other
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Inputs {
    /// The reflink probe beside the destination succeeded.
    pub probe_ok: bool,
    /// The destination already lies inside the configured volume.
    pub dest_in_volume: bool,
    /// Name of the volume configured for this repository, if any.
    pub volume: Option<String>,
    /// `auto_volume` for this repository (config); None: not chosen yet.
    pub auto_volume: Option<bool>,
    /// `--into-volume`.
    pub into_volume: bool,
    /// `--no-volume`.
    pub no_volume: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hint {
    None,
    /// A volume exists and the user has not chosen whether to use it, but
    /// there is no terminal to ask on.
    VolumeUndecided(String),
    /// Suggest `sharetree setup volume` (Linux).
    SetupVolume,
    /// Suggest `sharetree setup devdrive` (Windows).
    SetupDevDrive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Reflink at the requested destination. `seed`: the destination is in a
    /// sharetree volume, so make sure the volume's seed checkout exists and
    /// use it as the donor.
    Reflink { seed: bool },
    /// Relocate the worktree into the volume (mounting it if needed).
    IntoVolume(String),
    /// The destination cannot reflink and the user has not decided whether
    /// to use this volume: ask (terminal) or fall back to plain with
    /// `Hint::VolumeUndecided` (no terminal). See `resolve_ask`.
    Ask(String),
    /// Plain `git worktree add` at the requested destination.
    Plain(Hint),
    /// Refuse: the user asked for something that cannot be done.
    Fail(String),
}

pub fn decide(i: &Inputs, platform: Platform) -> Decision {
    if i.into_volume && i.no_volume {
        return Decision::Fail("--into-volume and --no-volume contradict each other".into());
    }
    let setup_hint = match platform {
        Platform::Linux => Hint::SetupVolume,
        Platform::Windows => Hint::SetupDevDrive,
        _ => Hint::None,
    };
    if i.no_volume {
        return if i.probe_ok {
            Decision::Reflink { seed: false }
        } else {
            Decision::Plain(Hint::None)
        };
    }
    if i.into_volume {
        return match &i.volume {
            None => Decision::Fail(match platform {
                Platform::Windows => {
                    "--into-volume: no Dev Drive is configured; run `sharetree setup devdrive` first"
                        .into()
                }
                Platform::Linux => {
                    "--into-volume: no sharetree volume is configured; run `sharetree setup volume` first"
                        .into()
                }
                _ => "--into-volume: sharetree volumes exist only on Linux and Windows".into(),
            }),
            Some(_) if i.dest_in_volume && i.probe_ok => Decision::Reflink { seed: true },
            Some(v) => Decision::IntoVolume(v.clone()),
        };
    }
    if i.probe_ok {
        return Decision::Reflink {
            seed: i.dest_in_volume && i.volume.is_some(),
        };
    }
    match (&i.volume, i.auto_volume) {
        (Some(v), _) if i.dest_in_volume => Decision::IntoVolume(v.clone()),
        (Some(v), Some(true)) => Decision::IntoVolume(v.clone()),
        // The user said "never": no nagging.
        (Some(_), Some(false)) => Decision::Plain(Hint::None),
        (Some(v), None) => Decision::Ask(v.clone()),
        (None, _) => Decision::Plain(setup_hint),
    }
}

/// The user's answer to "use the volume?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Use the volume this time.
    Once,
    /// Use it now and from now on (`auto_volume = true`).
    Always,
    /// Plain checkout this time; ask again next time.
    NotNow,
    /// Plain checkout now and from now on (`auto_volume = false`).
    Never,
}

impl Answer {
    pub fn use_volume(self) -> bool {
        matches!(self, Answer::Once | Answer::Always)
    }

    /// What to store as `auto_volume`, if anything.
    pub fn persist(self) -> Option<bool> {
        match self {
            Answer::Always => Some(true),
            Answer::Never => Some(false),
            Answer::Once | Answer::NotNow => None,
        }
    }
}

pub const ASK_CHOICES: &str = "[y]es this time / [a]lways / [n]ot this time / ne[v]er";

/// Parse a typed answer. Empty input (just Enter) means "not this time",
/// the choice that changes nothing.
pub fn parse_answer(s: &str) -> Option<Answer> {
    match s.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "once" => Some(Answer::Once),
        "a" | "always" => Some(Answer::Always),
        "" | "n" | "no" | "not" => Some(Answer::NotNow),
        "v" | "never" => Some(Answer::Never),
        _ => None,
    }
}

/// Turn `Decision::Ask` into a final decision. `answer` is the user's
/// reply when a terminal was available to ask on, None otherwise.
pub fn resolve_ask(volume: &str, answer: Option<Answer>) -> Decision {
    match answer {
        Some(a) if a.use_volume() => Decision::IntoVolume(volume.to_string()),
        Some(_) => Decision::Plain(Hint::None),
        None => Decision::Plain(Hint::VolumeUndecided(volume.to_string())),
    }
}

/// One-paragraph hint printed after a plain fallback.
pub fn hint_text(h: &Hint, fs: &str) -> Option<String> {
    match h {
        Hint::None => None,
        Hint::VolumeUndecided(v) => Some(format!(
            "hint: sharetree volume '{v}' is set up, but you have not chosen whether worktrees \
             of this repository go there, and there is no terminal to ask on, so it was not \
             used. Use it this time with `sharetree add --into-volume ...`; choose for good with \
             `sharetree setup auto on --repo` (or `off`; without --repo for every repository)"
        )),
        Hint::SetupVolume => Some(format!(
            "hint: {fs} cannot share blocks between files. `sharetree setup volume` creates a \
             sparse Btrfs image (no root needed on a desktop session); worktrees placed in it \
             cost only the files that differ"
        )),
        Hint::SetupDevDrive => Some(format!(
            "hint: {fs} cannot share blocks between files. `sharetree setup devdrive` (admin, \
             once) creates a ReFS Dev Drive for worktrees; sharetree does not block-clone on \
             Windows yet, but Dev Drive checkouts are faster"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inp() -> Inputs {
        Inputs::default()
    }

    #[test]
    fn reflink_probe_wins_when_nothing_else_asked() {
        let i = Inputs {
            probe_ok: true,
            volume: Some("default".into()),
            auto_volume: Some(true),
            ..inp()
        };
        assert_eq!(
            decide(&i, Platform::Linux),
            Decision::Reflink { seed: false },
            "a reflink-capable destination never moves into the volume automatically"
        );
    }

    #[test]
    fn destination_inside_the_volume_seeds() {
        let i = Inputs {
            probe_ok: true,
            dest_in_volume: true,
            volume: Some("default".into()),
            ..inp()
        };
        assert_eq!(
            decide(&i, Platform::Linux),
            Decision::Reflink { seed: true }
        );
        let i = Inputs {
            into_volume: true,
            ..i
        };
        assert_eq!(
            decide(&i, Platform::Linux),
            Decision::Reflink { seed: true }
        );
    }

    #[test]
    fn no_reflink_with_volume_asks_unless_chosen() {
        let mut i = Inputs {
            volume: Some("v".into()),
            ..inp()
        };
        assert_eq!(
            decide(&i, Platform::Linux),
            Decision::Ask("v".into()),
            "undecided: ask"
        );
        i.auto_volume = Some(true);
        assert_eq!(
            decide(&i, Platform::Linux),
            Decision::IntoVolume("v".into())
        );
        i.auto_volume = Some(false);
        assert_eq!(
            decide(&i, Platform::Linux),
            Decision::Plain(Hint::None),
            "never: plain, no hint"
        );
        i.auto_volume = None;
        i.into_volume = true;
        assert_eq!(
            decide(&i, Platform::Linux),
            Decision::IntoVolume("v".into())
        );
        // An explicit --into-volume wins even where reflinks work.
        i.probe_ok = true;
        assert_eq!(
            decide(&i, Platform::Linux),
            Decision::IntoVolume("v".into())
        );
    }

    #[test]
    fn no_volume_configured_hints_setup_per_platform() {
        assert_eq!(
            decide(&inp(), Platform::Linux),
            Decision::Plain(Hint::SetupVolume)
        );
        assert_eq!(
            decide(&inp(), Platform::Windows),
            Decision::Plain(Hint::SetupDevDrive)
        );
        assert_eq!(decide(&inp(), Platform::MacOs), Decision::Plain(Hint::None));
    }

    #[test]
    fn into_volume_without_volume_fails_loudly() {
        let i = Inputs {
            into_volume: true,
            ..inp()
        };
        match decide(&i, Platform::Linux) {
            Decision::Fail(m) => assert!(m.contains("sharetree setup volume"), "{m}"),
            d => panic!("{d:?}"),
        }
        match decide(&i, Platform::Windows) {
            Decision::Fail(m) => assert!(m.contains("sharetree setup devdrive"), "{m}"),
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn no_volume_flag_overrides_auto() {
        let i = Inputs {
            volume: Some("v".into()),
            no_volume: true,
            ..inp()
        };
        assert_eq!(decide(&i, Platform::Linux), Decision::Plain(Hint::None));
        let i = Inputs {
            into_volume: true,
            ..i
        };
        assert!(matches!(decide(&i, Platform::Linux), Decision::Fail(_)));
    }

    #[test]
    fn windows_dev_drive_destination_goes_through_placement() {
        // Windows has no clone engine yet: probe_ok is always false there.
        let i = Inputs {
            dest_in_volume: true,
            volume: Some("devdrive".into()),
            ..inp()
        };
        assert_eq!(
            decide(&i, Platform::Windows),
            Decision::IntoVolume("devdrive".into())
        );
    }

    #[test]
    fn flags_and_reflink_never_ask() {
        let base = Inputs {
            volume: Some("v".into()),
            ..inp()
        };
        for i in [
            Inputs {
                probe_ok: true,
                ..base.clone()
            },
            Inputs {
                into_volume: true,
                ..base.clone()
            },
            Inputs {
                no_volume: true,
                ..base.clone()
            },
            Inputs {
                dest_in_volume: true,
                ..base.clone()
            },
        ] {
            assert!(
                !matches!(decide(&i, Platform::Linux), Decision::Ask(_)),
                "{i:?}"
            );
        }
        // No volume configured: nothing to ask about.
        assert_eq!(
            decide(&inp(), Platform::Linux),
            Decision::Plain(Hint::SetupVolume)
        );
    }

    #[test]
    fn answers_parse_and_resolve() {
        assert_eq!(parse_answer("y\n"), Some(Answer::Once));
        assert_eq!(parse_answer(" A "), Some(Answer::Always));
        assert_eq!(
            parse_answer(""),
            Some(Answer::NotNow),
            "Enter changes nothing"
        );
        assert_eq!(parse_answer("n"), Some(Answer::NotNow));
        assert_eq!(parse_answer("never"), Some(Answer::Never));
        assert_eq!(parse_answer("maybe"), None);

        assert_eq!(Answer::Once.persist(), None);
        assert_eq!(Answer::NotNow.persist(), None);
        assert_eq!(Answer::Always.persist(), Some(true));
        assert_eq!(Answer::Never.persist(), Some(false));

        assert_eq!(
            resolve_ask("v", Some(Answer::Once)),
            Decision::IntoVolume("v".into())
        );
        assert_eq!(
            resolve_ask("v", Some(Answer::Always)),
            Decision::IntoVolume("v".into())
        );
        assert_eq!(
            resolve_ask("v", Some(Answer::NotNow)),
            Decision::Plain(Hint::None)
        );
        assert_eq!(
            resolve_ask("v", Some(Answer::Never)),
            Decision::Plain(Hint::None)
        );
        assert_eq!(
            resolve_ask("v", None),
            Decision::Plain(Hint::VolumeUndecided("v".into())),
            "no terminal: plain, with a hint"
        );
        let h = hint_text(&Hint::VolumeUndecided("v".into()), "ext4").unwrap();
        assert!(
            h.contains("--into-volume") && h.contains("setup auto on --repo"),
            "{h}"
        );
    }

    #[test]
    fn hints_name_the_command() {
        assert!(hint_text(&Hint::SetupVolume, "ext4")
            .unwrap()
            .contains("ext4 cannot share blocks"));
        assert!(hint_text(&Hint::SetupDevDrive, "NTFS")
            .unwrap()
            .contains("sharetree setup devdrive"));
        assert!(hint_text(&Hint::None, "x").is_none());
    }
}
