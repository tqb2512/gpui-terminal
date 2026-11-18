//! SSH transport and host verification.

mod client;
mod sftp;
mod target;

pub use client::{
    Event, Input, InputPermit, InputSender, PtySize, SendInputError, Session, SessionError, connect,
};
pub use sftp::{
    PublishMode, RemoteEntry, RemoteKind, RemoteMetadata, RemotePath, RemoteReader, SftpClient,
    SftpError, SftpOpener, StagedRemoteWrite,
};
pub use target::{AuthMethod, Endpoint, Target, TargetError};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_connect_normalizes_without_merging_dns_identities() -> Result<(), TargetError> {
        let target = Target::parse("ops@BUILD.Example")?;
        assert_eq!(target.username(), "ops");
        assert_eq!(target.endpoint().host(), "build.example");
        assert_eq!(target.endpoint().port(), 22);
        for other in ["ops@build.example.", "ops@alias", "ops@build.example:2222"] {
            assert_ne!(target.endpoint(), Target::parse(other)?.endpoint());
        }
        let ipv6 = Target::parse("ops@[2001:0DB8:0::1]:2222")?;
        assert_eq!(ipv6.endpoint().host(), "2001:db8::1");
        assert_eq!(ipv6.endpoint().port(), 2222);
        for invalid in [
            "host",
            "@host",
            "user@",
            "user@host:0",
            "user@host:x",
            "user@[host]",
            "user@host..",
            "user@a/b",
            "user@host:65536",
        ] {
            assert!(Target::parse(invalid).is_err(), "{invalid}");
        }
        Ok(())
    }
}
