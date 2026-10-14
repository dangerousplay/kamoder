# Vendored ONVIF schemas

The input of `./x gen-onvif`, which generates `../src/generated`. Every file
is a byte-for-byte copy of the upstream document, fetched 2026-10-10. The
layout mirrors the upstream URLs, so the WSDLs' relative imports
(`../../../ver10/schema/onvif.xsd`) and the generator's catalog
(`CATALOG` in `xtask/src/gen_onvif.rs`) agree on where everything lives.
Nothing is fetched at generation time.

| File | Upstream URL | Version | sha256 |
| --- | --- | --- | --- |
| `ver10/schema/onvif.xsd` | https://www.onvif.org/ver10/schema/onvif.xsd | ONVIF 25.12 | `1de6e9dd31a18a6773b611c4f7eaa0528f219098ddfc883b380802aa9a7ff647` |
| `ver10/schema/common.xsd` | https://www.onvif.org/ver10/schema/common.xsd (included by onvif.xsd) | ONVIF 25.06 | `d945394fe823febcd873ed8444ccfd73f3b5234d6dfbf0e151a33f5e611b5077` |
| `ver10/device/wsdl/devicemgmt.wsdl` | https://www.onvif.org/ver10/device/wsdl/devicemgmt.wsdl | ONVIF 26.06 | `c66773e3da37e493f370b1ed31906a8b3eee980ccd23735245ea2dc45b87eecf` |
| `ver10/media/wsdl/media.wsdl` | https://www.onvif.org/ver10/media/wsdl/media.wsdl | ONVIF 21.06 | `bb4596f47ff0f907b8113fd2c86f649faa098df0e0df9f1c5f4de34f592de1eb` |
| `ver20/ptz/wsdl/ptz.wsdl` | https://www.onvif.org/ver20/ptz/wsdl/ptz.wsdl | ONVIF 26.06 | `6cbeaf5c8d123401d4fe1a5b99bead9b31730fcf113078fa85776c3aefae4d2b` |
| `ver20/imaging/wsdl/imaging.wsdl` | https://www.onvif.org/ver20/imaging/wsdl/imaging.wsdl (fetched 2026-10-10, for IND-07) | ONVIF 19.06 | `02f222407d41b0a7aca13259603a85ab69ca1cc28899a47734abac4e3902e854` |
| `docs.oasis-open.org/wsn/b-2.xsd` | http://docs.oasis-open.org/wsn/b-2.xsd | OASIS WS-BaseNotification 1.3 | `26f0c7aa9a3719412e2e3275bc80320edb4b2dc9783bf2bb08c6378f4ca681fe` |
| `docs.oasis-open.org/wsn/t-1.xsd` | http://docs.oasis-open.org/wsn/t-1.xsd | OASIS WS-Topics 1.3 | `ad5db0432a3885d1c8a574ba6b4058193af29bc971232c4bb6b31f8295fe3329` |
| `docs.oasis-open.org/wsrf/bf-2.xsd` | http://docs.oasis-open.org/wsrf/bf-2.xsd | OASIS WS-BaseFaults 1.2 | `7863ccf5b21105c374a405968266fa18ecd0ebe0f7b9b19cd9378656229ff468` |
| `www.w3.org/2005/08/addressing/ws-addr.xsd` | http://www.w3.org/2005/08/addressing/ws-addr.xsd | W3C WS-Addressing 1.0 | `af806cc9af2514e9b0b07efe4e4dc42a32dad8ac08473dc025136207779d5ee9` |
| `www.w3.org/2001/xml.xsd` | http://www.w3.org/2001/xml.xsd | W3C XML namespace | `61960fb3131e38022caad5360e2f33a3382578ab3c80cd58bd74320ede61b20c` |
| `www.w3.org/2003/05/soap-envelope.xsd` | https://www.w3.org/2003/05/soap-envelope | W3C SOAP 1.2 | `3ae8caa9a74e83528cc0e1a59fc12784435e8e0086e890d0e1b04d881c079bec` |
| `www.w3.org/2005/05/xmlmime.xsd` | https://www.w3.org/2005/05/xmlmime | W3C XML Media Types | `330ab79ecdd3a8628f12ef4cbe34351e448f9b9231bacad339881ce1a61237a2` |
| `www.w3.org/2004/08/xop/include.xsd` | https://www.w3.org/2004/08/xop/include | W3C XOP 1.0 | `dff9100efce4afb21de0cf673fb7fad103672a169862ab0f12e34c312b20e60d` |

The ONVIF versions are the `version` attribute of each document's
`<xs:schema>`. The three W3C URLs without an extension are stored with
`.xsd` appended; their content is unchanged.

Verify with `sha256sum` from this directory, e.g.
`sha256sum ver10/schema/onvif.xsd`.

## Updating

Replace the files with new upstream copies (never edit them; see below),
update the table, run `./x gen-onvif`, and review the diff in
`src/generated`. A schema that imports a URL not yet in the catalog makes the
generator fail with that URL: vendor it here and add it to `CATALOG`.

## Licences

These files are **not** covered by this repository's GPL-3.0-or-later. Each
carries its own notice, which must stay with it:

- **ONVIF** (`ver10/`, `ver20/`): "Copyright (c) 2008-2026 by ONVIF: Open
  Network Video Interface Forum. All rights reserved. Recipients of this
  document may copy, distribute, publish, or display this document so long as
  this copyright notice, license and disclaimer are retained with all copies
  of the document. **No license is granted to modify this document.**"
  (followed by an as-is warranty disclaimer). Redistributing the unmodified
  files, as here, is permitted. They must therefore never be edited in
  place: the generator lifts the schema out of each WSDL in memory instead.
  The notice says nothing either way about code generated from the schemas;
  generating bindings is the documents' intended use, and the generated Rust
  contains no part of the documents' text except type and element names.
- **OASIS** (`docs.oasis-open.org/`): "Copyright (C) OASIS Open (2004-2006).
  All Rights Reserved." May be copied and distributed provided the notice is
  kept; the documents themselves may not be modified.
- **W3C** (`www.w3.org/`): soap-envelope, xmlmime and ws-addr are under the
  W3C Software License named in their headers (permissive, notice must be
  kept). xml.xsd and xop/include carry no notice in the file; they are W3C
  documents published for unrestricted use by schemas that import them.
