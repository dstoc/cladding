//! Host-file ownership checks trust the real UID of the account that invoked
//! Cladding. An elevated effective UID grants process access, but does not
//! change which user's files Cladding trusts. A validator can separately allow
//! root-owned system files when its path rules require them.

/// Return the UID to use when Cladding validates host-file ownership.
///
/// The real UID identifies the invoking user, including when it differs from
/// the effective UID. Use this for ownership checks and for host ownership
/// values passed to container builds.
pub fn invoking_uid() -> u32 {
    // Keep both process identities in the selection function so the rule can
    // be tested with differing real and effective UIDs.
    invoking_uid_from(unsafe { libc::getuid() }, unsafe { libc::geteuid() })
}

pub(crate) const fn invoking_uid_from(real_uid: u32, _effective_uid: u32) -> u32 {
    real_uid
}

#[cfg(test)]
mod tests {
    use super::invoking_uid_from;

    #[test]
    fn file_ownership_uses_real_uid_when_effective_uid_differs() {
        let real_uid = 1000;
        let effective_uid = 0;

        assert_eq!(invoking_uid_from(real_uid, effective_uid), real_uid);
        assert_ne!(invoking_uid_from(real_uid, effective_uid), effective_uid);
    }

    #[test]
    fn file_ownership_uses_same_uid_when_real_and_effective_match() {
        assert_eq!(invoking_uid_from(1000, 1000), 1000);
    }
}
