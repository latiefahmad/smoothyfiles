# Velopack 1.2.161, vendored with one change

This is the `velopack` crate 1.2.161 from crates.io (MIT, https://github.com/velopack/velopack), used through
`[patch.crates-io]` in the workspace's Cargo.toml.

The one change, in `src/download.rs` (`get_download_agent`): the HTTP agent verifies servers with ureq's platform verifier,
that is Windows' own certificate store and policy, instead of only the roots built into the program (webpki). Behind a proxy,
a filter or a company network that adds its own root certificate to Windows, the update check failed with "invalid peer
certificate: UnknownIssuer". `Cargo.toml` turns on ureq's `platform-verifier` feature for it.

To move to a newer Velopack: copy the new crate here and make the same change again (or drop this folder and the patch line
if Velopack lets the TLS roots be chosen by then).
