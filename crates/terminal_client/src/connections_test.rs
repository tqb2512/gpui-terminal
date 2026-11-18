use std::{io, path::PathBuf};

use hosts::{KeyCheck, Repository};
use ssh::{AuthMethod, Event, Target};
use tokio::sync::oneshot;
use zeroize::Zeroizing;

use crate::connections::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn spec(label: &str, address: &str, auth: AuthMethod) -> Result<ConnectionSpec, ssh::TargetError> {
    Ok(ConnectionSpec {
        label: label.into(),
        target: Target::parse(address)?,
        auth,
        profile_id: None,
    })
}

fn trust(
    core: &mut Core,
    attempt: AttemptId,
    key: &[u8],
) -> Result<(PromptId, oneshot::Receiver<bool>), Box<dyn std::error::Error>> {
    let (reply, response) = oneshot::channel();
    let prompt = core.on_event(
        attempt,
        Event::VerifyHost {
            key: key.to_vec(),
            fingerprint: "SHA256:fixture".into(),
            reply,
        },
    )?;
    let Some(ConnectionEventKind::Prompt(prompt)) = prompt else {
        return Err(io::Error::other("unknown public key should prompt before auth").into());
    };
    Ok((prompt.id, response))
}

#[test]
fn forget_cancels_pending_without_touching_established() -> TestResult {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("test.db");
    let repo = Repository::open(&path)?;
    let mut core = Core::new(repo);
    let host = spec("server", "ops@example.test", AuthMethod::Password)?;
    let established = core.insert_tab(host.clone())?;
    let (first_prompt, mut first_reply) = trust(&mut core, established.id, b"host")?;
    core.respond(first_prompt, PromptResponse::Trust(true))?;
    assert!(first_reply.try_recv()?);
    core.on_event(established.id, Event::Connected)?;

    let unknown = spec("unknown", "ops@unknown.test", AuthMethod::Password)?;
    let pending_trust = core.insert_tab(unknown.clone())?;
    let (old_prompt, mut old_trust_reply) = trust(&mut core, pending_trust.id, b"unknown")?;
    let affected = core.forget_host(unknown.target.endpoint())?;
    assert_eq!(affected, [pending_trust.id]);
    assert!(pending_trust.cancel.is_cancelled());
    assert!(
        core.respond(old_prompt, PromptResponse::Trust(true))
            .is_err()
    );
    assert!(old_trust_reply.try_recv().is_err());
    let store = Repository::open(&path)?;
    assert!(matches!(
        store.check_key(unknown.target.endpoint(), b"unknown")?,
        KeyCheck::Unknown { .. }
    ));

    let password = core.insert_tab(host.clone())?;
    let passphrase = core.insert_tab(host.clone())?;
    let other = core.insert_tab(unknown)?;
    let (password_sender, mut password_reply) = oneshot::channel();
    let (passphrase_sender, mut passphrase_reply) = oneshot::channel();
    let Some(ConnectionEventKind::Prompt(password_prompt)) = core.on_event(
        password.id,
        Event::Password {
            reply: password_sender,
        },
    )?
    else {
        return Err(io::Error::other("missing password prompt").into());
    };
    let Some(ConnectionEventKind::Prompt(passphrase_prompt)) = core.on_event(
        passphrase.id,
        Event::Passphrase {
            reply: passphrase_sender,
        },
    )?
    else {
        return Err(io::Error::other("missing passphrase prompt").into());
    };
    let affected = core.forget_host(host.target.endpoint())?;
    assert!(affected.contains(&password.id));
    assert!(affected.contains(&passphrase.id));
    assert!(!affected.contains(&established.id));
    for prompt in [password_prompt.id, passphrase_prompt.id] {
        assert!(
            core.respond(prompt, PromptResponse::Secret(Zeroizing::new("old".into())))
                .is_err()
        );
    }
    assert!(password_reply.try_recv().is_err());
    assert!(passphrase_reply.try_recv().is_err());
    assert!(password.cancel.is_cancelled());
    assert!(passphrase.cancel.is_cancelled());
    assert!(!established.cancel.is_cancelled());
    assert!(!other.cancel.is_cancelled());
    Ok(())
}

#[test]
fn simultaneous_different_key_approval_rejects_loser_before_auth() -> TestResult {
    let mut core = Core::new(Repository::open(std::path::Path::new(":memory:"))?);
    let host = spec("server", "ops@example.test", AuthMethod::Password)?;
    let first = core.insert_tab(host.clone())?;
    let second = core.insert_tab(host)?;
    let (first_prompt, mut first_reply) = trust(&mut core, first.id, b"first")?;
    let (second_prompt, mut second_reply) = trust(&mut core, second.id, b"second")?;
    core.respond(first_prompt, PromptResponse::Trust(true))?;
    assert!(matches!(
        core.respond(second_prompt, PromptResponse::Trust(true)),
        Err(ManagerError::ChangedKey(_))
    ));
    assert!(first_reply.try_recv()?);
    assert!(!second_reply.try_recv()?);
    Ok(())
}

#[test]
fn closing_a_tab_discards_its_prompt() -> TestResult {
    let mut core = Core::new(Repository::open(std::path::Path::new(":memory:"))?);
    let first = core.insert_tab(spec("first", "ops@host.test", AuthMethod::Password)?)?;
    let (prompt, mut reply) = trust(&mut core, first.id, b"host")?;
    core.close(first.id.tab)?;
    assert!(first.cancel.is_cancelled());
    assert!(core.respond(prompt, PromptResponse::Trust(true)).is_err());
    assert!(reply.try_recv().is_err());
    Ok(())
}

#[test]
fn stale_reply_cannot_answer_reconnect() -> TestResult {
    let mut core = Core::new(Repository::open(std::path::Path::new(":memory:"))?);
    let first = core.insert_tab(spec("first", "ops@host.test", AuthMethod::Password)?)?;
    let (old_prompt, mut old_reply) = trust(&mut core, first.id, b"host")?;
    let retry = core.reconnect(first.id.tab)?;
    let (fresh_prompt, mut fresh_reply) = trust(&mut core, retry.id, b"host")?;
    assert_ne!(old_prompt, fresh_prompt);
    assert!(
        core.respond(old_prompt, PromptResponse::Trust(true))
            .is_err()
    );
    assert!(old_reply.try_recv().is_err());
    assert!(matches!(
        fresh_reply.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    core.respond(fresh_prompt, PromptResponse::Trust(true))?;
    assert!(fresh_reply.try_recv()?);
    Ok(())
}

#[test]
fn reconnect_preserves_target_and_remembers_only_tab_key_path() -> TestResult {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("hosts.db");
    let mut repo = Repository::open(&path)?;
    let mut original = spec(
        "server",
        "ops@example.test",
        AuthMethod::PrivateKey { path: None },
    )?;
    let profile = hosts::ProfileDraft {
        label: original.label.clone(),
        target: original.target.clone(),
        auth: original.auth.clone(),
    };
    let id = repo.create(&profile)?;
    original.profile_id = Some(id);
    let mut core = Core::new(repo);
    let first = core.insert_tab(original.clone())?;
    let (sender, mut reply) = oneshot::channel();
    let Some(ConnectionEventKind::Prompt(prompt)) =
        core.on_event(first.id, Event::KeyPath { reply: sender })?
    else {
        return Err(io::Error::other("missing path prompt").into());
    };
    let selected = PathBuf::from("fixture-key");
    core.respond(prompt.id, PromptResponse::KeyPath(selected.clone()))?;
    assert_eq!(reply.try_recv()?, selected);
    let mut external = Repository::open(&path)?;
    assert_eq!(
        external.list("")?[0].draft.auth,
        AuthMethod::PrivateKey { path: None }
    );
    external.delete(id)?;
    assert!(matches!(
        core.on_event(first.id, Event::Connected)?,
        Some(ConnectionEventKind::Connected { recent_error: None })
    ));
    let retry = core.reconnect(first.id.tab)?;
    assert_ne!(retry.id, first.id);
    assert_eq!(retry.spec.target, original.target);
    assert_eq!(
        retry.spec.auth,
        AuthMethod::PrivateKey {
            path: Some(selected)
        }
    );
    assert!(
        core.respond(
            prompt.id,
            PromptResponse::Secret(Zeroizing::new("old".into()))
        )
        .is_err()
    );
    Ok(())
}
