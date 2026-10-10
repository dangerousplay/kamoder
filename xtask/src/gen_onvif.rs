//! `./x gen-onvif`: generates `onvif-schema/src/generated` from the vendored
//! ONVIF WSDL/XSD with xsd-parser.
//!
//! The output is committed rather than produced by a build script, so the
//! generator (xsd-parser and ~80 crates, ~2 s) never runs in a camera build
//! and every change to the types shows up as a reviewable diff.
//! `./x gen-onvif --check` regenerates in memory and fails if the committed
//! files differ; `./x ci` runs it.
//!
//! # Pipeline
//!
//! 1. xsd-parser reads XSD only, not WSDL. ONVIF defines its request and
//!    response elements inside each WSDL's `<wsdl:types>`, so the resolver
//!    lifts that `<xs:schema>` out ([`extract_schema`]) and hands it over as if
//!    it were a standalone file at the WSDL's URL.
//! 2. Every location is resolved through [`CATALOG`], which maps the upstream
//!    URL to the vendored copy. Relative imports (`../../../ver10/schema/
//!    onvif.xsd`) are joined against the upstream URL first, so they land on
//!    catalog entries too. Nothing is fetched; an unknown URL is an error.
//! 3. Only the request and response elements of [`SERVICES`] and what they
//!    reference are generated. All of onvif.xsd would be several times larger
//!    for types the camera never sends or reads.
//! 4. Each generated namespace module becomes its own file, formatted with the
//!    same rustfmt `./x fmt` uses, so `./x fmt --check` agrees with the output.

use crate::tasks::{rustfmt_command, workspace_root};
use eyre::{bail, eyre, Result, WrapErr};
use proc_macro2::TokenStream;
use quick_xml::events::Event;
use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use url::Url;
use xsd_parser::config::{
    Config, Generate, GeneratorFlags, IdentQuadruple, InterpreterFlags, NamespaceIdent,
    OptimizerFlags, RenderStep,
};
use xsd_parser::pipeline::parser::resolver::{ResolveRequest, ResolveResult, Resolver};
use xsd_parser::pipeline::parser::Parser;
use xsd_parser::pipeline::renderer::NamespaceSerialization;
use xsd_parser::{
    exec_generator, exec_interpreter, exec_optimizer, exec_render, IdentType, SubModules,
};
use xsd_parser_types::misc::{Namespace, NamespacePrefix};

/// One ONVIF service: its WSDL, target namespace, and the operations whose
/// `<Op>` request and `<Op>Response` elements are generated.
struct Service {
    wsdl: &'static str,
    namespace: &'static str,
    operations: &'static [&'static str],
}

/// The operations the camera's ONVIF server implements (Profile S plus PTZ
/// and the Imaging IR-cut filter).
///
/// This is the single list that decides what gets generated. Add an operation
/// here, run `./x gen-onvif`, and its request/response types appear in
/// `onvif_schema::{tds, trt, tptz, timg}` along with everything they reference.
const SERVICES: &[Service] = &[
    Service {
        wsdl: "https://www.onvif.org/ver10/device/wsdl/devicemgmt.wsdl",
        namespace: "http://www.onvif.org/ver10/device/wsdl",
        operations: &[
            "GetSystemDateAndTime",
            "GetDeviceInformation",
            "GetCapabilities",
            "GetServices",
            "GetScopes",
        ],
    },
    Service {
        wsdl: "https://www.onvif.org/ver10/media/wsdl/media.wsdl",
        namespace: "http://www.onvif.org/ver10/media/wsdl",
        operations: &[
            "GetProfiles",
            "GetStreamUri",
            "GetSnapshotUri",
            "GetVideoSources",
            "GetVideoSourceConfiguration",
            "GetVideoSourceConfigurations",
            "GetVideoEncoderConfiguration",
            "GetVideoEncoderConfigurations",
            "GetVideoEncoderConfigurationOptions",
            "GetCompatibleVideoEncoderConfigurations",
            "GetAudioSources",
        ],
    },
    Service {
        wsdl: "https://www.onvif.org/ver20/ptz/wsdl/ptz.wsdl",
        namespace: "http://www.onvif.org/ver20/ptz/wsdl",
        operations: &[
            "GetNodes",
            "GetNode",
            "GetConfigurations",
            "GetConfiguration",
            "GetConfigurationOptions",
            "GetStatus",
            "ContinuousMove",
            "Stop",
            "AbsoluteMove",
            "RelativeMove",
            "GetPresets",
            "SetPreset",
            "GotoPreset",
            "RemovePreset",
            "GotoHomePosition",
        ],
    },
    // Only the IR-cut filter is served (IND-07): day/night through the
    // lights owner. GetServiceCapabilities is left out: the library routes by
    // action name only, and Device/Media share it.
    Service {
        wsdl: "https://www.onvif.org/ver20/imaging/wsdl/imaging.wsdl",
        namespace: "http://www.onvif.org/ver20/imaging/wsdl",
        operations: &["GetImagingSettings", "SetImagingSettings", "GetOptions"],
    },
];

/// Upstream URL → file under `onvif-schema/schema`. Keep in step with the
/// table in `onvif-schema/schema/README.md`.
///
/// Some URLs are spelled `https://` here because that is how onvif.xsd
/// imports them; the catalog matches the exact string, after relative
/// locations have been joined against the importing document's URL.
const CATALOG: &[(&str, &str)] = &[
    (
        "https://www.onvif.org/ver10/schema/onvif.xsd",
        "ver10/schema/onvif.xsd",
    ),
    (
        "https://www.onvif.org/ver10/schema/common.xsd",
        "ver10/schema/common.xsd",
    ),
    (
        "https://www.onvif.org/ver10/device/wsdl/devicemgmt.wsdl",
        "ver10/device/wsdl/devicemgmt.wsdl",
    ),
    (
        "https://www.onvif.org/ver10/media/wsdl/media.wsdl",
        "ver10/media/wsdl/media.wsdl",
    ),
    (
        "https://www.onvif.org/ver20/ptz/wsdl/ptz.wsdl",
        "ver20/ptz/wsdl/ptz.wsdl",
    ),
    (
        "https://www.onvif.org/ver20/imaging/wsdl/imaging.wsdl",
        "ver20/imaging/wsdl/imaging.wsdl",
    ),
    (
        "http://docs.oasis-open.org/wsn/b-2.xsd",
        "docs.oasis-open.org/wsn/b-2.xsd",
    ),
    (
        "http://docs.oasis-open.org/wsn/t-1.xsd",
        "docs.oasis-open.org/wsn/t-1.xsd",
    ),
    (
        "http://docs.oasis-open.org/wsrf/bf-2.xsd",
        "docs.oasis-open.org/wsrf/bf-2.xsd",
    ),
    (
        "http://www.w3.org/2005/08/addressing/ws-addr.xsd",
        "www.w3.org/2005/08/addressing/ws-addr.xsd",
    ),
    ("http://www.w3.org/2001/xml.xsd", "www.w3.org/2001/xml.xsd"),
    (
        "https://www.w3.org/2003/05/soap-envelope",
        "www.w3.org/2003/05/soap-envelope.xsd",
    ),
    (
        "https://www.w3.org/2005/05/xmlmime",
        "www.w3.org/2005/05/xmlmime.xsd",
    ),
    (
        "https://www.w3.org/2004/08/xop/include",
        "www.w3.org/2004/08/xop/include.xsd",
    ),
];

/// Prefixes the generated serializers declare. Pinned to the names every
/// ONVIF client and spec example uses, rather than whatever xsd-parser would
/// derive from the schema that happened to be read first.
const PREFIXES: &[(&str, &str)] = &[
    ("tt", "http://www.onvif.org/ver10/schema"),
    ("tds", "http://www.onvif.org/ver10/device/wsdl"),
    ("trt", "http://www.onvif.org/ver10/media/wsdl"),
    ("tptz", "http://www.onvif.org/ver20/ptz/wsdl"),
    ("timg", "http://www.onvif.org/ver20/imaging/wsdl"),
];

const XS: &str = "http://www.w3.org/2001/XMLSchema";

/// First line of every generated file.
const HEADER: &str = "// @generated by `./x gen-onvif` from onvif-schema/schema. Do not edit.\n";

fn schema_dir() -> PathBuf {
    workspace_root().join("onvif-schema/schema")
}

fn output_dir() -> PathBuf {
    workspace_root().join("onvif-schema/src/generated")
}

pub fn gen_onvif(extra: &[String]) -> Result<()> {
    let check = match extra {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => bail!("usage: ./x gen-onvif [--check]"),
    };
    let started = std::time::Instant::now();
    let files = generate(&schema_dir())?;
    let out = output_dir();

    if check {
        let committed = read_tree(&out)?;
        let mut problems = Vec::new();
        for (path, code) in &files {
            match committed.get(path) {
                None => problems.push(format!("  missing  {}", path.display())),
                Some(old) if old != code => problems.push(format!("  differs  {}", path.display())),
                Some(_) => {}
            }
        }
        for path in committed.keys().filter(|p| !files.contains_key(*p)) {
            problems.push(format!("  stale    {}", path.display()));
        }
        if !problems.is_empty() {
            bail!(
                "{} is out of date with the schemas or the generator:\n{}\nrun ./x gen-onvif",
                out.display(),
                problems.join("\n")
            );
        }
        println!(
            "{} matches the generator ({} files, {:.1?})",
            out.display(),
            files.len(),
            started.elapsed()
        );
        return Ok(());
    }

    // Replace the whole tree so a module that disappears from the output
    // does not linger as a stale file.
    if out.exists() {
        std::fs::remove_dir_all(&out)
            .wrap_err_with(|| format!("cannot clear {}", out.display()))?;
    }
    let mut bytes = 0;
    for (path, code) in &files {
        let target = out.join(path);
        std::fs::create_dir_all(target.parent().expect("a file has a directory"))?;
        std::fs::write(&target, code)
            .wrap_err_with(|| format!("cannot write {}", target.display()))?;
        bytes += code.len();
    }
    println!(
        "generated {} ({} files, {bytes} bytes, {:.1?})",
        out.display(),
        files.len(),
        started.elapsed()
    );
    Ok(())
}

/// Runs xsd-parser over the schemas in `root` and returns every generated
/// file, formatted, keyed by its path relative to `src/generated`.
fn generate(root: &Path) -> Result<BTreeMap<PathBuf, String>> {
    let mut parser = Parser::new()
        .with_resolver(Catalog {
            root: root.to_owned(),
        })
        .resolve_includes(true)
        .generate_prefixes(true)
        .alternative_prefixes(false)
        .with_default_namespaces();
    for (prefix, namespace) in PREFIXES {
        parser = parser.with_namespace(
            NamespacePrefix::new(prefix.as_bytes().to_vec()),
            Namespace::new(namespace.as_bytes().to_vec()),
        );
    }

    let mut wanted = Vec::new();
    for service in SERVICES {
        let url = Url::parse(service.wsdl)?;
        parser = parser
            .add_schema_from_url(url)
            .map_err(|e| eyre!("parsing {}: {e}", service.wsdl))?;
        for operation in service.operations {
            for name in [operation.to_string(), [operation, "Response"].concat()] {
                let mut ident = IdentQuadruple::new(name, IdentType::Element);
                ident.ns = Some(NamespaceIdent::Namespace(Namespace::new(
                    service.namespace.as_bytes().to_vec(),
                )));
                wanted.push(ident);
            }
        }
    }
    let schemas = parser.finish();

    let mut config = Config::default();
    config.interpreter.flags = InterpreterFlags::all();
    // `xs:anySimpleType` is mapped to the runtime crate's type explicitly:
    // with the default mapping xsd-parser 1.5.2 emits code that does not
    // compile for an attribute of that type.
    let mut config = config
        .with_xs_any_simple_type("xsd_parser_types::xml::AnySimpleType")
        .with_quick_xml_serialize_config(NamespaceSerialization::Global, None)
        .with_quick_xml_deserialize()
        .with_render_step(RenderStep::NamespaceConstants);
    // REMOVE_DUPLICATES merges structurally equal types under one name, which
    // would rename ONVIF types away from their schema names.
    config.optimizer.flags = OptimizerFlags::all() - OptimizerFlags::REMOVE_DUPLICATES;
    // Not `GeneratorFlags::all()`: ADVANCED_ENUMS without EnumConstants does
    // not compile in 1.5.2. These four are what the types need: one module
    // per namespace, flattened content, `xs:any` support (ONVIF extension
    // points), and fully qualified std paths so nothing clashes with schema
    // names like `Option` or `Result`.
    config.generator.flags = GeneratorFlags::USE_NAMESPACE_MODULES
        | GeneratorFlags::FLATTEN_CONTENT
        | GeneratorFlags::ANY_TYPE_SUPPORT
        | GeneratorFlags::BUILD_IN_ABSOLUTE_PATHS;
    config.generator.generate = Generate::Types(wanted);

    let meta = exec_interpreter(config.interpreter, &schemas).map_err(|e| eyre!("{e}"))?;
    let meta = exec_optimizer(config.optimizer, meta).map_err(|e| eyre!("{e}"))?;
    let data = exec_generator(config.generator, &schemas, &meta).map_err(|e| eyre!("{e}"))?;
    let module = exec_render(config.renderer, &data).map_err(|e| eyre!("{e}"))?;

    // One file per module, as `Module::write_to_files` lays them out, but kept
    // in memory so `--check` can compare without touching the tree.
    let mut files = BTreeMap::new();
    module
        .write_to_files_with(|module, path| {
            let mut code = TokenStream::new();
            module.to_code(&mut code, SubModules::Files);
            let file = if module.modules.is_empty() {
                path.with_extension("rs")
            } else {
                path.join("mod.rs")
            };
            let code = [HEADER, &code.to_string()].concat();
            files.insert(file, rustfmt(&code)?);
            Ok::<_, eyre::Report>(())
        })
        .wrap_err("rendering the generated modules")?;
    Ok(files)
}

/// Formats one file through rustfmt's stdin, with the toolchain `./x fmt`
/// uses, so the committed output is already `cargo fmt` clean.
fn rustfmt(code: &str) -> Result<String> {
    let mut child = rustfmt_command()
        .args(["--edition", "2021", "--emit", "stdout"])
        .current_dir(workspace_root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .wrap_err("cannot run rustfmt")?;
    // Written from a thread: rustfmt starts emitting before it has read all
    // of a large file, and both pipes would fill up otherwise.
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = code.to_owned();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output()?;
    writer.join().expect("rustfmt writer panicked")?;
    if !output.status.success() {
        bail!("rustfmt failed with {}", output.status);
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Every file under `dir`, keyed by its path relative to `dir`.
fn read_tree(dir: &Path) -> Result<BTreeMap<PathBuf, String>> {
    fn walk(base: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, String>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(base, &path, files)?;
            } else {
                let relative = path.strip_prefix(base)?.to_owned();
                files.insert(relative, std::fs::read_to_string(&path)?);
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    if dir.is_dir() {
        walk(dir, dir, &mut files)?;
    }
    Ok(files)
}

/// An XML catalog: resolves schema locations to the vendored copies.
#[derive(Debug)]
struct Catalog {
    root: PathBuf,
}

impl Resolver for Catalog {
    type Buffer = Cursor<Vec<u8>>;
    type Error = std::io::Error;

    fn resolve(&mut self, request: &ResolveRequest) -> ResolveResult<Self> {
        let url = match &request.current_location {
            Some(current) => current.join(&request.requested_location),
            None => Url::parse(&request.requested_location),
        }
        .map_err(std::io::Error::other)?;
        let Some((_, local)) = CATALOG.iter().find(|(known, _)| *known == url.as_str()) else {
            return Err(std::io::Error::other(format!(
                "{url} is not in the catalog; vendor it under onvif-schema/schema \
                 and add it to CATALOG in xtask/src/gen_onvif.rs"
            )));
        };
        let path = self.root.join(local);
        let bytes = std::fs::read(&path)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        let bytes = if local.ends_with(".wsdl") {
            extract_schema(&bytes)
                .map_err(|e| std::io::Error::other(format!("{}: {e}", path.display())))?
        } else {
            bytes
        };
        let name = Path::new(local)
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_owned);
        Ok(Some((name, url, Cursor::new(bytes))))
    }
}

/// Returns the `<xs:schema>` inside a WSDL's `<wsdl:types>` as a standalone
/// document.
///
/// The schema inherits namespace declarations from `<wsdl:definitions>` (all
/// ONVIF WSDLs declare `xs:` or their service prefix only there), so
/// the in-scope declarations it does not repeat itself are added to its start
/// tag. Everything else is copied event by event, unchanged.
fn extract_schema(wsdl: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(wsdl)?;
    let document = roxmltree::Document::parse(text)?;
    let schema = document
        .descendants()
        .find(|node| node.has_tag_name((XS, "schema")))
        .ok_or_else(|| eyre!("no <xs:schema> in the WSDL"))?;

    let mut reader = quick_xml::Reader::from_str(&text[schema.range()]);
    let mut writer = quick_xml::Writer::new(Vec::new());
    let Event::Start(start) = reader.read_event()? else {
        bail!("<xs:schema> does not start with a start tag");
    };
    let mut start = start.into_owned();
    let declared: Vec<Option<Vec<u8>>> = start
        .attributes()
        .filter_map(|attribute| {
            let attribute = attribute.ok()?;
            match attribute.key.as_namespace_binding()? {
                quick_xml::name::PrefixDeclaration::Default => Some(None),
                quick_xml::name::PrefixDeclaration::Named(prefix) => Some(Some(prefix.to_vec())),
            }
        })
        .collect();
    for namespace in schema.namespaces() {
        let prefix = namespace.name().map(|p| p.as_bytes().to_vec());
        // `xml` is bound implicitly and must not be redeclared.
        if namespace.name() == Some("xml") || declared.contains(&prefix) {
            continue;
        }
        let key = match namespace.name() {
            Some(prefix) => ["xmlns:", prefix].concat(),
            None => "xmlns".to_owned(),
        };
        start.push_attribute((key.as_str(), namespace.uri()));
    }
    writer.write_event(Event::Start(start))?;
    loop {
        match reader.read_event()? {
            Event::Eof => break,
            event => writer.write_event(event)?,
        }
    }
    Ok(writer.into_inner())
}
