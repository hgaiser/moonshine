# Pairing, identity and revocation

Pairing approval is a host operator action. Open the loopback HTTP `/pin` page
linked in the server log, check the requester and certificate fingerprint, then
enter the PIN displayed by the client. The approval token applies to that exact
request; replacing a client ID requires a fresh approval. A remote client cannot
approve itself. For a headless host, forward the HTTP port over SSH as described
in [Configuration](CONFIGURATION.md#webserver).

## Pending transactions

The server admits at most 32 pending pairings. Client IDs are limited to 256
bytes and certificates to 16 KiB. Approval has five minutes; the complete
transaction expires one minute later, including requests abandoned after PIN
submission. A single sweep task removes expired transactions within another
second. Expiry, replacement and cancellation wake the approval waiter. Disconnect
before approval removes its request, and completed or failed approved protocol steps remove
their transaction. Approved requests abandoned between protocol steps expire.
Older approval waiters cannot remove replacement requests. Challenges and PINs
are accepted once per transaction.

Desktop notifications are coalesced to at most one per 30 seconds, with at most
one notification worker. They do not wait for a click. Use the logged loopback
URL when notifications are unavailable or coalesced. Global shutdown clears
pending requests. New approval is required after restart.

## Durable trust and migration

The existing data-directory `moonshine/state.toml` remains the source of trust.
`unique_id`, legacy `clients`, and `paired_certs` survive migration. New approved
pairings also store `client_certs`, a client-ID-to-fingerprint-set association.
An ID may have several certificates; one certificate may have several IDs.
The server does not guess associations between legacy sets. Legacy certificates
continue working; a newly approved pairing records its explicit association.
A legacy client ID alone does not prove ownership of any certificate.

Pair completion and revocation each replace the entire state file in one
serialized transaction. A private staging file is fully written and synced,
renamed over the old state, and the containing directory is synced before
publishing trust in memory or reporting success. An interrupted update leaves a
complete old or complete new file. The production service holds an exclusive
`state.lock` writer lock; do not run multiple servers against one state directory.

If persistence fails, authorization is disabled until restart. Repair disk space,
ownership, permissions or filesystem errors first. If a directory sync failed
after rename, the replacement may already be present; inspect the complete state
before retrying. Keep a protected backup of state and the matching TLS identity.
Stop the service before restoring a known-good backup. Do not delete state to
repair a parse error: that discards the server UUID and unrelated trust. A damaged
legacy file needs restoration from backup; migration cannot reconstruct missing
records. Restoring an old backup can restore revoked trust: reapply revocations
before exposing the server.

## Revoke a client

The host operator uses **POST** to the loopback HTTP `/unpair` route, with the
same peer, Host and browser-origin checks as PIN approval:

```sh
curl --fail -X POST 'http://localhost:47989/unpair?uniqueid=CLIENT_ID'
# For legacy state, or to remove one specific certificate:
curl --fail -X POST 'http://localhost:47989/unpair?fingerprint=SHA256_HEX'
```

URL-encode client IDs. Supply one target selector; a fingerprint takes precedence
if both are supplied. ID revocation removes all explicitly associated
certificates. Fingerprint revocation removes that certificate and its known ID
relationships. IDs still associated with other certificates remain paired.
Legacy unassociated IDs remain in the compatibility set when a fingerprint is
revoked, but cannot authorize HTTPS. ID-only revocation of an unassociated legacy
client fails with a request to use its fingerprint. Read `paired_certs` in the
protected state file to find legacy fingerprints; do not infer ownership from
set ordering. To revoke a known credential even when its legacy ID is ambiguous,
use its fingerprint. An unknown/already revoked target reports failure.

A paired client can also send **GET /unpair over HTTPS** for self-revocation.
The TLS certificate selects the credential: caller-supplied IDs/fingerprints
cannot revoke another client. Unauthenticated remote HTTP deletion is rejected,
and HTTP GET no longer reports fictitious success. Older clients that issue an
unauthenticated GET during pairing cleanup must retry pairing normally.

Revocation clears pending approvals, drains in-flight launch/resume/cancel
operations, and stops the active stream before returning success. Sessions do
not yet record the TLS credential owner, so **any revocation stops any active
stream**, including another client's. Other paired certificates remain valid
and may launch again. If teardown fails, trust remains revoked but the operation
reports failure. Existing TLS connections recheck trust on each protected
request. Revoked credentials cannot launch, resume or call other protected HTTPS
routes after restart without another operator-approved pairing.

## TLS identity permissions and recovery

Generated keys, recovery journals and staging files use mode **0600**, including
under umask 000 or 022. Packaged systemd services also use `UMask=0077`. Existing
valid certificate/key files are loaded without replacement or chmod, including
custom administrator-managed paths. Broad existing key permissions produce an
operator warning; have the key's administrator set appropriate ownership and
mode 0600, accounting for any external certificate renewal workflow.

Initial creation uses an exclusive kernel lock at `PRIVATE_KEY.creation.lock`
and a private, synced `PRIVATE_KEY.creation.toml` recovery journal containing
the complete intended certificate/key pair. Publication never overwrites an
existing file. After interruption, startup completes only missing files from
the journal, requiring any existing file to match, then removes the journal.
Never publish or print this journal: it contains the private key. The lock file
is retained so concurrent creators always lock the same inode.

A partial externally provided identity without a journal, an incomplete pair
conflicting with its journal, or an invalid existing pair causes startup failure. Restore its matching file
from backup or complete the external deployment; the server will not silently
rotate a usable deployed identity. Provisioning does not follow dangling target
symlinks or recovery/lock symlinks. Usable administrator-managed certificate/key
symlinks remain supported. Interrupted temporary files are private and are not
authoritative; with the service stopped they can be removed after identifying
them. Keep identity directories owned by the service user and inaccessible to
untrusted writers.
