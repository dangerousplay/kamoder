# Local patches to xsd-parser-types 0.2.1

This directory is the crates.io source of `xsd-parser-types` 0.2.1
(MIT, <https://github.com/Bergmann89/xsd-parser>, `xsd-parser-types/` at
b76fa2fb4aa47c1e88e620ba34c8b7a6947f95c2), copied verbatim from the cargo
registry and wired into the workspace through `[patch.crates-io]` in the root
`Cargo.toml`. Everything here except the change below is upstream code
(`Cargo.toml.orig` is upstream's manifest, untouched; the registry
`Cargo.lock` was dropped).

**This fork is temporary.** Drop it (delete this directory and its
`[patch.crates-io]` entry) as soon as an `xsd-parser-types` release depends on
`quick-xml >= 0.41`; bump the exact pins in `crates/onvif-schema/Cargo.toml`
and `xtask/Cargo.toml` together and re-run `./x gen-onvif`.

## 1. quick-xml 0.38 -> 0.41

File: `Cargo.toml` (`[dependencies.quick-xml] version`).

quick-xml 0.38.4 carries RUSTSEC-2026-0194 (quadratic duplicate-attribute
check) and RUSTSEC-2026-0195 (unbounded namespace-declaration allocation in
`NsReader`), both fixed only in 0.41.0. The ONVIF service parses every SOAP
request body from the network through this crate (`onvif_schema::from_xml`),
and `./x deny` refuses known advisories without an allow-list. The crate
compiles unchanged against 0.41; the ONVIF unit tests cover the round trip.

The code generator in `xtask` (host-only, its own lockfile) still uses the
registry 0.2.1 with quick-xml 0.38: it only reads the vendored WSDL/XSD, and
its output does not depend on the quick-xml version.
