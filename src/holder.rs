//! Who is holding a schema, and whether they are still there.

/// A claim on a schema: which machine, which boot of it, which process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub host: String,
    pub boot: String,
    pub pid: u32,
}

impl Holder {
    pub fn me() -> Holder {
        Holder {
            host: hostname(),
            boot: boot_id(),
            pid: std::process::id(),
        }
    }

    pub fn parse(text: &str) -> Option<Holder> {
        let mut parts = text.split('/');
        let host = parts.next()?.to_owned();
        let boot = parts.next()?.to_owned();
        let pid = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Holder { host, boot, pid })
    }

    /// Whether this claim belongs to a process that has finished.
    ///
    /// `None` when there is no way to know: another machine, or the same one
    /// since restarted. A pid on its own would say yes here and be wrong —
    /// the numbers come round again, and a live process would be robbed of a
    /// schema it is in the middle of using. What cannot be answered is left
    /// to the lease's age instead.
    pub fn gone(&self) -> Option<bool> {
        let me = Holder::me();
        if self.host != me.host || self.boot != me.boot {
            return None;
        }
        if self.pid == me.pid {
            return Some(false);
        }
        let proc = std::path::Path::new("/proc");
        if !proc.is_dir() {
            return None;
        }
        Some(!proc.join(self.pid.to_string()).exists())
    }
}

impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.host, self.boot, self.pid)
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_owned())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Changes on every restart, so a pid from before one is never trusted.
fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|id| id.trim().replace('-', ""))
        .ok()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "noboot".to_owned())
}

#[cfg(test)]
mod tests {
    use super::Holder;

    #[test]
    fn a_claim_survives_being_written_down() {
        let me = Holder::me();
        assert_eq!(Holder::parse(&me.to_string()), Some(me));
    }

    #[test]
    fn nonsense_is_not_a_claim() {
        assert!(Holder::parse("").is_none());
        assert!(Holder::parse("host/boot").is_none());
        assert!(Holder::parse("host/boot/notapid").is_none());
        assert!(Holder::parse("host/boot/1/extra").is_none());
    }

    #[test]
    fn this_process_is_not_gone() {
        assert_eq!(Holder::me().gone(), Some(false));
    }

    #[test]
    fn another_machine_cannot_be_judged() {
        let elsewhere = Holder {
            host: "somewhere-else".to_owned(),
            boot: "0".to_owned(),
            pid: 1,
        };
        assert_eq!(elsewhere.gone(), None);
    }

    /// The bug this exists to prevent: a pid from before a restart matching a
    /// live one, and a running test having its schema taken away.
    #[test]
    fn a_pid_from_another_boot_is_not_judged_by_this_boot() {
        let me = Holder::me();
        let before_the_restart = Holder {
            boot: format!("{}x", me.boot),
            ..me
        };
        assert_eq!(before_the_restart.gone(), None);
    }
}
