use crate::config::Volume;

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
pub struct Inputs<'a> {
    pub probe_ok: bool,
    pub dest_in_volume: bool,
    pub volume: Option<&'a Volume>,
    pub auto_volume: Option<bool>,
    pub into_volume: bool,
    pub no_volume: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hint {
    None,
    VolumeUndecided(String),
    SetupVolume,
    SetupDevDrive,
}

#[derive(Debug, PartialEq)]
pub enum Decision<'a> {
    Reflink,
    Volume(&'a Volume),
    Ask(&'a Volume),
    Plain(Hint),
    Fail(String),
}

pub fn decide<'a>(i: &Inputs<'a>, platform: Platform) -> Decision<'a> {
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
            Decision::Reflink
        } else {
            Decision::Plain(Hint::None)
        };
    }
    if i.into_volume {
        return match i.volume {
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
            Some(v) => Decision::Volume(v),
        };
    }
    if let Some(v) = i.volume.filter(|_| i.dest_in_volume) {
        return Decision::Volume(v);
    }
    if i.probe_ok {
        return Decision::Reflink;
    }
    match (i.volume, i.auto_volume) {
        (Some(v), Some(true)) => Decision::Volume(v),
        (Some(_), Some(false)) => Decision::Plain(Hint::None),
        (Some(v), None) => Decision::Ask(v),
        (None, _) => Decision::Plain(setup_hint),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Once,
    Always,
    NotNow,
    Never,
}

impl Answer {
    pub fn use_volume(self) -> bool {
        matches!(self, Answer::Once | Answer::Always)
    }

    pub fn persist(self) -> Option<bool> {
        match self {
            Answer::Always => Some(true),
            Answer::Never => Some(false),
            Answer::Once | Answer::NotNow => None,
        }
    }
}

pub const ASK_CHOICES: &str = "[y]es this time / [a]lways / [n]ot this time / ne[v]er";

pub fn parse_answer(s: &str) -> Option<Answer> {
    match s.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "once" => Some(Answer::Once),
        "a" | "always" => Some(Answer::Always),
        "" | "n" | "no" | "not" => Some(Answer::NotNow),
        "v" | "never" => Some(Answer::Never),
        _ => None,
    }
}

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

    use crate::config::{MountMethod, VolumeKind};

    fn volume(name: &str) -> Volume {
        Volume {
            name: name.into(),
            kind: VolumeKind::Btrfs,
            image: "test.img".into(),
            label: "sharetree".into(),
            size: 1 << 30,
            mount: MountMethod::Udisks,
            mountpoint: None,
            unit: None,
        }
    }

    fn inp() -> Inputs<'static> {
        Inputs::default()
    }

    #[test]
    fn reflink_probe_wins_when_nothing_else_asked() {
        let v = volume("default");
        let i = Inputs {
            probe_ok: true,
            volume: Some(&v),
            auto_volume: Some(true),
            ..inp()
        };
        assert_eq!(
            decide(&i, Platform::Linux),
            Decision::Reflink,
            "a reflink-capable destination never moves into the volume automatically"
        );
    }

    #[test]
    fn destination_inside_the_volume_uses_volume_placement() {
        let v = volume("default");
        let i = Inputs {
            probe_ok: true,
            dest_in_volume: true,
            volume: Some(&v),
            ..inp()
        };
        assert_eq!(decide(&i, Platform::Linux), Decision::Volume(&v));
        let i = Inputs {
            into_volume: true,
            ..i
        };
        assert_eq!(decide(&i, Platform::Linux), Decision::Volume(&v));
    }

    #[test]
    fn no_reflink_with_volume_asks_unless_chosen() {
        let v = volume("v");
        let mut i = Inputs {
            volume: Some(&v),
            ..inp()
        };
        assert_eq!(
            decide(&i, Platform::Linux),
            Decision::Ask(&v),
            "undecided: ask"
        );
        i.auto_volume = Some(true);
        assert_eq!(decide(&i, Platform::Linux), Decision::Volume(&v));
        i.auto_volume = Some(false);
        assert_eq!(
            decide(&i, Platform::Linux),
            Decision::Plain(Hint::None),
            "never: plain, no hint"
        );
        i.auto_volume = None;
        i.into_volume = true;
        assert_eq!(decide(&i, Platform::Linux), Decision::Volume(&v));
        i.probe_ok = true;
        assert_eq!(decide(&i, Platform::Linux), Decision::Volume(&v));
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
        let v = volume("v");
        let i = Inputs {
            volume: Some(&v),
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
        let v = volume("devdrive");
        let i = Inputs {
            dest_in_volume: true,
            volume: Some(&v),
            ..inp()
        };
        assert_eq!(decide(&i, Platform::Windows), Decision::Volume(&v));
    }

    #[test]
    fn flags_and_reflink_never_ask() {
        let v = volume("v");
        let base = Inputs {
            volume: Some(&v),
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
        assert_eq!(
            decide(&inp(), Platform::Linux),
            Decision::Plain(Hint::SetupVolume)
        );
    }

    #[test]
    fn answers_parse_and_define_current_and_future_choices() {
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

        assert!(Answer::Once.use_volume());
        assert!(Answer::Always.use_volume());
        assert!(!Answer::NotNow.use_volume());
        assert!(!Answer::Never.use_volume());

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
