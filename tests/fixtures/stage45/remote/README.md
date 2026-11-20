# Remote leaf characterization (TRU-274, Stage 5 PR 25a)

`requests.json` pins the exact protobuf payloads captured by the disposable
loopback fixture: three handshakes, a directory listing, and a streamed read.
The handshake sends `GitTerm V6 desktop`, host version `6.0.0`, and protocol 1.
The trace passed against the original host implementation before extraction
and against the core implementation behind the same host facades afterward.

`tests/remote_leaf_parity.rs` also pins:

- Repeated handshakes keep saved remote identity and endpoint; two saved IDs
  at one endpoint remain distinct. Agent name/version/home come from the reply.
- Capabilities preserve order, duplicates, and unknown names. Source operation
  flags keep their existing local/remote values.
- Each operation re-reads the token reference; emptying the disposable token
  file fails before another request is sent.
- Remote request paths remain opaque, including `Q:/` and `..`; authoritative
  listing paths and parents come from the response. Remote paths have no local
  filesystem representation.
- Stream chunks preserve arbitrary bytes, full size, and truncation. A source
  rejects the opposite path variant. Existing source tests preserve local
  reads/listings, sorting/filtering, and variant-only ownership (local paths
  are not newly confined to the configured root).

`tests/support/remote_fixture.rs` is shared by host and core tests. Its chat
index and previews are synthetic and do not scan real harness directories.
The agent server implementation and phone authority remain host-owned.

`crates/gitterm-core/tests/remote.rs` checks the source adapters directly and
pins saved config tags, field order, defaults, omissions, token references,
and opaque location JSON with and without the runtime feature.
