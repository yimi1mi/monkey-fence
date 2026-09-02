//! fake 平台身份(T4d 契约驱动)。

use super::{OsIdentity, PlatformIdentity};

pub struct FakeIdentity {
    pub current: OsIdentity,
    pub elevated: bool,
}

impl PlatformIdentity for FakeIdentity {
    fn current(&self) -> OsIdentity {
        self.current.clone()
    }

    fn peer_matches(&self, expected: &OsIdentity, actual: &OsIdentity) -> bool {
        expected.user_sid == actual.user_sid && expected.pid == actual.pid
    }

    fn is_elevated(&self) -> bool {
        self.elevated
    }
}
