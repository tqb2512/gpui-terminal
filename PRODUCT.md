# Product

<!-- impeccable:product-schema 1 -->

## Platform

Windows x64 desktop

## Users

People who connect to and manage multiple remote hosts over SSH from a Windows PC.

## Product Purpose

Make it easy to return to saved hosts, switch among independent SSH sessions, and work with remote files without losing sight of the shell.

## Operating Context

The main screen lists saved hosts. Open sessions live in tabs across the top. Each connected host can show a Files panel on the left; the terminal narrows to keep its text unobstructed.

## Capabilities and Constraints

- The app runs locally without an account or a system SSH client. It uses embedded SSH and a remote shell.
- Host keys are verified before authentication. Passwords, passphrases, private key material, and terminal transcripts are not persisted.
- SFTP uses the tab's authenticated connection for browsing, individual file transfers, and supported local editing with save-back.

See [project scope](terminal-client-project.md) for architectural and release constraints.
