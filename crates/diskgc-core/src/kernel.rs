//! The minimum kernel (docs/safety.md, Path traversal): Linux 5.8, the first
//! with `STATX_MNT_ID` and `STATX_ATTR_MOUNT_ROOT` (`openat2` is 5.6).
//! Older kernels exit 2 at startup instead of failing later in the middle
//! of a reclaim.

use std::fmt;

use crate::host::{Host, HostError};

/// A kernel's `major.minor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct KernelVersion {
    pub major: u32,
    pub minor: u32,
}

impl fmt::Display for KernelVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// The oldest supported kernel.
pub const MIN_KERNEL: KernelVersion = KernelVersion { major: 5, minor: 8 };

/// Why diskgc will not run on this kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelError {
    TooOld {
        release: String,
        version: KernelVersion,
    },
    /// The release does not start with `major.minor`: fail closed.
    Unparseable {
        release: String,
    },
    Host(HostError),
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooOld { release, version } => write!(
                f,
                "Linux {version} (release {release:?}) is too old: diskgc needs Linux \
                 {MIN_KERNEL} or newer (statx mount ids, openat2)"
            ),
            Self::Unparseable { release } => write!(
                f,
                "cannot tell the kernel version from release {release:?}; diskgc needs Linux \
                 {MIN_KERNEL} or newer"
            ),
            Self::Host(e) => write!(f, "cannot read the kernel release: {e}"),
        }
    }
}

impl std::error::Error for KernelError {}

/// The `major.minor` at the start of a release string such as
/// `6.1.0-13-amd64` or `5.8.0`.
pub fn parse_release(release: &str) -> Option<KernelVersion> {
    let mut parts = release.splitn(3, '.');
    let number = |s: Option<&str>| -> Option<u32> {
        let digits: &str = s?;
        let end = digits
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(digits.len());
        digits[..end].parse().ok()
    };
    let major = parts
        .next()
        .filter(|s| s.chars().all(|c| c.is_ascii_digit()));
    Some(KernelVersion {
        major: number(major)?,
        minor: number(parts.next())?,
    })
}

/// Checks a release string against [`MIN_KERNEL`].
pub fn check_release(release: &str) -> Result<KernelVersion, KernelError> {
    let version = parse_release(release).ok_or_else(|| KernelError::Unparseable {
        release: release.to_owned(),
    })?;
    if version < MIN_KERNEL {
        return Err(KernelError::TooOld {
            release: release.to_owned(),
            version,
        });
    }
    Ok(version)
}

/// Checks the host's kernel against [`MIN_KERNEL`].
pub fn require_supported<H: Host>(host: &H) -> Result<KernelVersion, KernelError> {
    let release = host.kernel_release().map_err(KernelError::Host)?;
    check_release(&release)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u32, minor: u32) -> KernelVersion {
        KernelVersion { major, minor }
    }

    #[test]
    fn releases_parse_to_major_minor() {
        assert_eq!(parse_release("6.1.0-13-amd64"), Some(v(6, 1)));
        assert_eq!(parse_release("5.8.0"), Some(v(5, 8)));
        assert_eq!(parse_release("7.2.9"), Some(v(7, 2)));
        assert_eq!(parse_release("5.10"), Some(v(5, 10)));
        assert_eq!(parse_release("6.12rc1"), Some(v(6, 12)));
        // What personality(UNAME26) makes uname report on a 7.2 kernel.
        assert_eq!(parse_release("2.6.62"), Some(v(2, 6)));
        for bad in ["", "6", "x.y", "6.", ".8", "v6.1", "6-1"] {
            assert_eq!(parse_release(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn kernels_older_than_5_8_are_refused() {
        for old in [
            "5.7.19",
            "5.4.0-150-generic",
            "4.19.0",
            "2.6.62",
            "3.10.0-1160.el7",
        ] {
            let err = check_release(old).unwrap_err();
            assert!(matches!(err, KernelError::TooOld { .. }), "{old}: {err}");
            assert!(err.to_string().contains("Linux 5.8 or newer"), "{err}");
        }
        for ok in ["5.8.0", "5.10.0", "6.1.0-13-amd64", "7.2.9", "10.0.0"] {
            assert!(check_release(ok).is_ok(), "{ok}");
        }
        assert!(matches!(
            check_release("unknown"),
            Err(KernelError::Unparseable { .. })
        ));
    }
}
