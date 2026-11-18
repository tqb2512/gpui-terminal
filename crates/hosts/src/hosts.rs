//! Saved host profiles and approved host keys.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use ssh::{AuthMethod, Endpoint, Target};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProfileId(i64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileDraft {
    pub label: String,
    pub target: Target,
    pub auth: AuthMethod,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Profile {
    pub id: ProfileId,
    pub draft: ProfileDraft,
    pub last_connected: Option<i64>,
}

pub struct Repository(Connection);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyCheck {
    Unknown { revision: i64 },
    Match,
    Changed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Approval {
    Approved,
    Changed,
    Stale,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("Could not read or update saved SSH connections.")]
    Sql(#[from] rusqlite::Error),
    #[error("This saved-connections database uses an unsupported version.")]
    Version,
    #[error("The selected saved host no longer exists.")]
    MissingProfile,
    #[error("Enter a profile name between 1 and 256 characters.")]
    InvalidLabel,
    #[error("A saved SSH connection has invalid data.")]
    InvalidProfile,
    #[error("The system clock is outside the supported range.")]
    Clock,
    #[error("The SSH host trust record is invalid.")]
    Trust,
}

impl Repository {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(3))?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match transaction.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))? {
            0 => {
                transaction.execute_batch(include_str!("../migrations/0001_init.sql"))?;
                transaction.pragma_update(None, "user_version", 1)?;
            }
            1 => {}
            _ => return Err(StoreError::Version),
        }
        transaction.commit()?;
        Ok(Self(connection))
    }

    pub fn create(&mut self, profile: &ProfileDraft) -> Result<ProfileId, StoreError> {
        let fields = profile_fields(profile)?;
        let endpoint = profile.target.endpoint();
        Ok(ProfileId(self.0.query_row(
            "INSERT INTO profiles (label, host, port, username, auth, key_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING id",
            params![
                fields.0,
                endpoint.host(),
                endpoint.port(),
                profile.target.username(),
                fields.1,
                fields.2
            ],
            |row| row.get(0),
        )?))
    }

    pub fn update(&mut self, id: ProfileId, profile: &ProfileDraft) -> Result<(), StoreError> {
        let fields = profile_fields(profile)?;
        let endpoint = profile.target.endpoint();
        if self.0.execute(
            "UPDATE profiles SET label=?1, host=?2, port=?3, username=?4, auth=?5, key_path=?6 WHERE id=?7",
            params![fields.0, endpoint.host(), endpoint.port(), profile.target.username(), fields.1, fields.2, id.0],
        )? == 0 {
            return Err(StoreError::MissingProfile);
        }
        Ok(())
    }

    pub fn delete(&mut self, id: ProfileId) -> Result<(), StoreError> {
        if self.0.execute("DELETE FROM profiles WHERE id=?1", [id.0])? == 0 {
            return Err(StoreError::MissingProfile);
        }
        Ok(())
    }

    pub fn list(&self, search: &str) -> Result<Vec<Profile>, StoreError> {
        let mut query = self.0.prepare(
            "SELECT id, label, host, port, username, auth, key_path, last_connected
             FROM profiles ORDER BY last_connected DESC, label COLLATE NOCASE, id",
        )?;
        let mut rows = query.query([])?;
        let search = search.trim().to_lowercase();
        let mut profiles = Vec::new();
        while let Some(row) = rows.next()? {
            let label: String = row.get(1)?;
            let host: String = row.get(2)?;
            let username: String = row.get(4)?;
            if ![&label, &host, &username]
                .iter()
                .any(|field| field.to_lowercase().contains(&search))
            {
                continue;
            }
            let endpoint =
                Endpoint::new(&host, row.get(3)?).map_err(|_| StoreError::InvalidProfile)?;
            let target =
                Target::new(endpoint, &username).map_err(|_| StoreError::InvalidProfile)?;
            let auth = match row.get::<_, String>(5)?.as_str() {
                "password" => AuthMethod::Password,
                "key" => AuthMethod::PrivateKey {
                    path: row.get::<_, Option<String>>(6)?.map(PathBuf::from),
                },
                _ => return Err(StoreError::InvalidProfile),
            };
            profiles.push(Profile {
                id: ProfileId(row.get(0)?),
                draft: ProfileDraft {
                    label,
                    target,
                    auth,
                },
                last_connected: row.get(7)?,
            });
        }
        Ok(profiles)
    }

    pub fn record_connected(&mut self, id: ProfileId) -> Result<(), StoreError> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StoreError::Clock)?
            .as_millis();
        let timestamp = i64::try_from(millis).map_err(|_| StoreError::Clock)?;
        self.0.execute(
            "UPDATE profiles SET last_connected=MAX(?1, (SELECT COALESCE(MAX(last_connected), 0) FROM profiles) + 1) WHERE id=?2",
            params![timestamp, id.0],
        )?;
        Ok(())
    }

    pub fn check_key(&self, endpoint: &Endpoint, key: &[u8]) -> Result<KeyCheck, StoreError> {
        let (revision, trusted) = trust_record(&self.0, endpoint)?;
        Ok(match trusted {
            None => KeyCheck::Unknown { revision },
            Some(bytes) if bytes == key => KeyCheck::Match,
            Some(_) => KeyCheck::Changed,
        })
    }

    pub fn approve_key(
        &mut self,
        endpoint: &Endpoint,
        key: &[u8],
        revision: i64,
    ) -> Result<Approval, StoreError> {
        if key.is_empty() {
            return Err(StoreError::Trust);
        }
        let transaction = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current, trusted) = trust_record(&transaction, endpoint)?;
        if current != revision {
            return Ok(Approval::Stale);
        }
        if let Some(trusted) = trusted {
            return Ok(if trusted == key {
                Approval::Approved
            } else {
                Approval::Changed
            });
        }
        transaction.execute(
            "INSERT INTO host_trust (host, port, revision, public_key) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (host, port) DO UPDATE SET public_key=excluded.public_key",
            params![endpoint.host(), endpoint.port(), revision, key],
        )?;
        transaction.commit()?;
        Ok(Approval::Approved)
    }

    pub fn forget_key(&mut self, endpoint: &Endpoint) -> Result<(), StoreError> {
        self.0.execute(
            "INSERT INTO host_trust (host, port, revision) VALUES (?1, ?2, 1)
             ON CONFLICT (host, port) DO UPDATE SET revision=revision+1, public_key=NULL",
            params![endpoint.host(), endpoint.port()],
        )?;
        Ok(())
    }
}

fn profile_fields(profile: &ProfileDraft) -> Result<(&str, &str, Option<&str>), StoreError> {
    let label = profile.label.trim();
    if label.is_empty() || label.chars().count() > 256 || label.chars().any(char::is_control) {
        return Err(StoreError::InvalidLabel);
    }
    let (auth, path) = match &profile.auth {
        AuthMethod::Password => ("password", None),
        AuthMethod::PrivateKey { path } => (
            "key",
            path.as_ref()
                .map(|path| {
                    path.to_str()
                        .filter(|path| !path.is_empty())
                        .ok_or(StoreError::InvalidProfile)
                })
                .transpose()?,
        ),
    };
    Ok((label, auth, path))
}

fn trust_record(
    connection: &Connection,
    endpoint: &Endpoint,
) -> Result<(i64, Option<Vec<u8>>), StoreError> {
    Ok(connection
        .query_row(
            "SELECT revision, public_key FROM host_trust WHERE host=?1 AND port=?2",
            params![endpoint.host(), endpoint.port()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or((0, None)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(label: &str, endpoint: &str) -> Result<ProfileDraft, ssh::TargetError> {
        Ok(ProfileDraft {
            label: label.into(),
            target: Target::parse(endpoint)?,
            auth: AuthMethod::PrivateKey {
                path: Some("id_ed25519".into()),
            },
        })
    }

    #[test]
    fn profiles_survive_database_reopen() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("hosts.db");
        let mut store = Repository::open(&path)?;
        let original = draft("A temporary label", "dev@build.example")?;
        let updated = draft("Production 100%", "deploy@build.example:2222")?;
        let id = store.create(&original)?;
        let other = store.create(&draft("Other", "dev@other.example")?)?;
        store.update(id, &updated)?;
        store.record_connected(id)?;
        drop(store);

        let mut recovered = Repository::open(&path)?;
        for search in ["production", "BUILD.EXAMPLE", "DePloY", "%"] {
            let rows = recovered.list(search)?;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].id, id);
            assert_eq!(rows[0].draft, updated);
            assert!(rows[0].last_connected.is_some());
        }
        let ids: Vec<_> = recovered.list("")?.into_iter().map(|row| row.id).collect();
        assert_eq!(ids, [id, other]);
        recovered.delete(id)?;
        recovered.record_connected(id)?;
        assert!(recovered.list("production")?.is_empty());
        Ok(())
    }

    #[test]
    fn approval_never_overwrites_a_different_key() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("hosts.db");
        let mut first = Repository::open(&path)?;
        let mut second = Repository::open(&path)?;
        let a = draft("Ops", "ops@build.example")?;
        let b = draft("Deploy", "deploy@BUILD.example")?;
        first.create(&a)?;
        second.create(&b)?;
        let a = a.target.endpoint();
        let b = b.target.endpoint();
        assert_eq!(
            first.check_key(a, b"first")?,
            KeyCheck::Unknown { revision: 0 }
        );
        assert_eq!(
            second.check_key(b, b"second")?,
            KeyCheck::Unknown { revision: 0 }
        );
        assert_eq!(first.approve_key(a, b"first", 0)?, Approval::Approved);
        assert_eq!(second.approve_key(b, b"second", 0)?, Approval::Changed);
        drop(first);
        drop(second);
        let reopened = Repository::open(&path)?;
        assert_eq!(reopened.check_key(b, b"first")?, KeyCheck::Match);
        assert_eq!(reopened.check_key(b, b"second")?, KeyCheck::Changed);
        Ok(())
    }

    #[test]
    fn forget_rejects_old_revision() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("hosts.db");
        let mut first = Repository::open(&path)?;
        let mut second = Repository::open(&path)?;
        let host = Endpoint::new("host", 22)?;
        assert_eq!(
            first.check_key(&host, b"key")?,
            KeyCheck::Unknown { revision: 0 }
        );
        second.forget_key(&host)?;
        assert_eq!(first.approve_key(&host, b"key", 0)?, Approval::Stale);
        assert_eq!(
            first.check_key(&host, b"key")?,
            KeyCheck::Unknown { revision: 1 }
        );
        assert_eq!(first.approve_key(&host, b"key", 1)?, Approval::Approved);
        second.forget_key(&host)?;
        assert_eq!(
            first.check_key(&host, b"key")?,
            KeyCheck::Unknown { revision: 2 }
        );
        Ok(())
    }
}
