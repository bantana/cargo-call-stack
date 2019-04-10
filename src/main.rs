#![deny(warnings)]

use core::{
    cmp,
    fmt::{self, Write as _},
    ops, str,
};
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    env,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{self, Command},
    time::SystemTime,
};

use ar::Archive;
use cargo_project::{Artifact, Profile, Project};
use clap::{crate_authors, crate_version, App, Arg};
use env_logger::{Builder, Env};
use failure::{bail, format_err};
use filetime::FileTime;
use log::{error, warn};
use petgraph::{
    algo,
    graph::{DiGraph, NodeIndex},
    visit::{Dfs, Reversed, Topo},
    Direction, Graph,
};
use walkdir::WalkDir;
use xmas_elf::{sections::SectionData, symbol_table::Entry, ElfFile};

use crate::{
    ir::{FnSig, Item, ItemMetadata, MetadataKind, Stmt, Type},
    thumb::Tag,
};

mod ir;
mod thumb;

// prevent myself from using some data structures when `-Z call-metadata` is present / absent
struct Maybe<T> {
    inner: T,
    panic_on_deref: bool,
}

impl<T> Maybe<T> {
    fn new(inner: T, panic_on_deref: bool) -> Self {
        Maybe {
            inner,
            panic_on_deref,
        }
    }

    fn into_iter(self) -> T {
        assert!(!self.panic_on_deref, "BUG: `panic_on_deref` is `true`");

        self.inner
    }
}

impl<T> ops::Deref for Maybe<T> {
    type Target = T;

    fn deref(&self) -> &T {
        assert!(!self.panic_on_deref, "BUG: `panic_on_deref` is `true`");

        &self.inner
    }
}

impl<T> ops::DerefMut for Maybe<T> {
    fn deref_mut(&mut self) -> &mut T {
        assert!(!self.panic_on_deref, "BUG: `panic_on_deref` is `true`");

        &mut self.inner
    }
}

fn main() -> Result<(), failure::Error> {
    match run() {
        Ok(ec) => process::exit(ec),
        Err(e) => {
            eprintln!("error: {}", e);
            process::exit(1)
        }
    }
}

// Font used in the dot graphs
const FONT: &str = "monospace";

// Version we analyzed to extract some ad-hoc information
const VERS: &str = "1.33.0"; // compiler-builtins = "0.1.4"

// check if the `-Z call-metadata` flag is supported by the current `rustc` version
fn probe_call_metadata() -> Result<bool, failure::Error> {
    let output = Command::new("rustc").args(&["-Z", "help"]).output()?;

    for line in str::from_utf8(&output.stdout)?.lines() {
        if line.contains("call-metadata") {
            return Ok(true);
        }
    }

    Ok(false)
}

fn is_tag(name: &str) -> bool {
    name == "$a" || name == "$t" || name == "$d" || {
        (name.starts_with("$a.") || name.starts_with("$d.") || name.starts_with("$t."))
            && name.splitn(2, '.').nth(1).unwrap().parse::<u64>().is_ok()
    }
}

// `i1 (_*, %"core::fmt::Formatter"*)`
fn sig_is_any_formatter_result(sig: &FnSig) -> bool {
    match (&sig.inputs[..], sig.output.as_ref()) {
        ([Type::Pointer(..), Type::Pointer(formatter)], Some(output))
            if **formatter == Type::Alias("core::fmt::Formatter")
                && **output == Type::Integer(1) =>
        {
            true
        }

        _ => false,
    }
}

fn sig_is_void_formatter_result(sig: &FnSig) -> bool {
    match (&sig.inputs[..], sig.output.as_ref()) {
        ([Type::Pointer(void), Type::Pointer(formatter)], Some(output))
            if **formatter == Type::Alias("core::fmt::Formatter")
                && **output == Type::Integer(1) =>
        {
            match **void {
                Type::Alias(void) => void.contains("fmt::Void"),
                _ => false,
            }
        }

        _ => false,
    }
}

fn run() -> Result<i32, failure::Error> {
    Builder::from_env(Env::default().default_filter_or("warn")).init();

    let matches = App::new("cargo-call-stack")
        .version(crate_version!())
        .author(crate_authors!(", "))
        .about("Generate a call graph and perform whole program stack usage analysis")
        // as this is used as a Cargo subcommand the first argument will be the name of the binary
        // we ignore this argument
        .arg(Arg::with_name("binary-name").hidden(true))
        .arg(
            Arg::with_name("target")
                .long("target")
                .takes_value(true)
                .value_name("TRIPLE")
                .help("Target triple for which the code is compiled"),
        )
        .arg(
            Arg::with_name("verbose")
                .long("verbose")
                .short("v")
                .help("Use verbose output"),
        )
        .arg(
            Arg::with_name("example")
                .long("example")
                .takes_value(true)
                .value_name("NAME")
                .help("Build only the specified example"),
        )
        .arg(
            Arg::with_name("bin")
                .long("bin")
                .takes_value(true)
                .value_name("BIN")
                .help("Build only the specified binary"),
        )
        .arg(
            Arg::with_name("features")
                .long("features")
                .takes_value(true)
                .value_name("FEATURES")
                .help("Space-separated list of features to activate"),
        )
        .arg(
            Arg::with_name("all-features")
                .long("all-features")
                .takes_value(false)
                .help("Activate all available features"),
        )
        .arg(
            Arg::with_name("START").help("consider only the call graph that starts from this node"),
        )
        .get_matches();
    let is_example = matches.is_present("example");
    let is_binary = matches.is_present("bin");
    let verbose = matches.is_present("verbose");
    let target_flag = matches.value_of("target");
    let profile = Profile::Release;

    let file;
    match (is_example, is_binary) {
        (true, false) => file = matches.value_of("example").unwrap(),
        (false, true) => file = matches.value_of("bin").unwrap(),
        _ => {
            return Err(failure::err_msg(
                "Please specify either --example <NAME> or --bin <NAME>.",
            ));
        }
    }

    let mut cargo = Command::new("cargo");
    cargo.arg("rustc");

    // NOTE we do *not* use `project.target()` here because Cargo will figure things out on
    // its own (i.e. it will search and parse .cargo/config, etc.)
    if let Some(target) = target_flag {
        cargo.args(&["--target", target]);
    }

    if matches.is_present("all-features") {
        cargo.arg("--all-features");
    } else if let Some(features) = matches.value_of("features") {
        cargo.args(&["--features", features]);
    }

    if is_example {
        cargo.args(&["--example", file]);
    }

    if is_binary {
        cargo.args(&["--bin", file]);
    }

    if profile.is_release() {
        cargo.arg("--release");
    }

    cargo.args(&[
        "--",
        // .ll file
        "--emit=llvm-ir,obj",
        // needed to produce a single .ll file
        "-C",
        "lto",
        // stack size information
        "-Z",
        "emit-stack-sizes",
    ]);

    let has_call_metadata = probe_call_metadata()?;
    if has_call_metadata {
        cargo.args(&["-Z", "call-metadata"]);
    }

    let cwd = env::current_dir()?;
    let project = Project::query(cwd)?;

    // "touch" some source file to trigger a rebuild
    let root = project.toml().parent().expect("UNREACHABLE");
    let now = FileTime::from_system_time(SystemTime::now());
    if !filetime::set_file_times(root.join("src/main.rs"), now, now).is_ok() {
        if !filetime::set_file_times(root.join("src/lib.rs"), now, now).is_ok() {
            // look for some rust source file and "touch" it
            let src = root.join("src");
            let haystack = if src.exists() { &src } else { root };

            for entry in WalkDir::new(haystack) {
                let entry = entry?;
                let path = entry.path();

                if path.extension().map(|ext| ext == "rs").unwrap_or(false) {
                    filetime::set_file_times(path, now, now)?;
                    break;
                }
            }
        }
    }

    if verbose {
        eprintln!("{:?}", cargo);
    }

    let status = cargo.status()?;

    if !status.success() {
        return Ok(status.code().unwrap_or(1));
    }

    let meta = rustc_version::version_meta()?;
    let host = meta.host;

    let mut path: PathBuf = if is_example {
        project.path(Artifact::Example(file), profile, target_flag, &host)?
    } else {
        project.path(Artifact::Bin(file), profile, target_flag, &host)?
    };

    let elf = fs::read(&path)?;

    // load llvm-ir file
    let mut ll = None;
    // most recently modified
    let mut mrm = SystemTime::UNIX_EPOCH;
    let prefix = format!("{}-", file.replace('-', "_"));

    path = path.parent().expect("unreachable").to_path_buf();

    if is_binary {
        path = path.join("deps"); // the .ll file is placed in ../deps
    }

    for e in fs::read_dir(path)? {
        let e = e?;
        let p = e.path();

        if p.extension().map(|e| e == "ll").unwrap_or(false) {
            if p.file_stem()
                .expect("unreachable")
                .to_str()
                .expect("unreachable")
                .starts_with(&prefix)
            {
                let modified = e.metadata()?.modified()?;
                if ll.is_none() {
                    ll = Some(p);
                    mrm = modified;
                } else {
                    if modified > mrm {
                        ll = Some(p);
                        mrm = modified;
                    }
                }
            }
        }
    }

    let ll = ll.expect("unreachable");
    let obj = ll.with_extension("o");
    let ll = fs::read_to_string(ll)?;
    let obj = fs::read(obj)?;

    let items = crate::ir::parse(&ll)?;
    let mut defines = HashMap::new();
    let mut declares = HashMap::new();
    // what does e.g. `!rust !0` mean
    let mut meta_defs = Maybe::new(BTreeMap::new(), !has_call_metadata);
    for item in items {
        match item {
            Item::Define(def) => {
                defines.insert(def.name, def);
            }

            Item::Declare(decl) => {
                declares.insert(decl.name, decl);
            }

            Item::Metadata(ItemMetadata::Unnamed { id, kind }) => {
                if has_call_metadata {
                    match kind {
                        MetadataKind::Set(set) => {
                            if !set.is_empty() {
                                meta_defs.insert(id, MetadataKind::Set(set));
                            }
                        }

                        _ => {
                            meta_defs.insert(id, kind);
                        }
                    }
                }
            }

            _ => {}
        }
    }

    // functions that could be called by `ArgumentV1.formatter`
    let mut formatter_callees_ = Maybe::new(HashSet::new(), !has_call_metadata);
    // functions that belong to the meta group `!rust !0`
    let mut meta_groups = Maybe::new(BTreeMap::<_, Vec<_>>::new(), !has_call_metadata);
    // now we do a pass over the `define`s to sort them in meta groups
    if has_call_metadata {
        for (name, def) in &defines {
            let looks_like_formatter_callee = sig_is_any_formatter_result(&def.sig);

            for meta in &def.meta {
                if meta.kind == "rust" {
                    let id = meta.id;

                    let meta_kind = meta_defs.get(&id).unwrap_or_else(|| {
                        panic!("BUG: metadata `!{}` doesn't appear to be call metadata", id)
                    });

                    match meta_kind {
                        MetadataKind::Set(set) => {
                            for id in set {
                                meta_groups.entry(*id).or_default().push(name);
                            }
                        }

                        MetadataKind::Fn { .. } => {
                            if looks_like_formatter_callee {
                                formatter_callees_.insert(name);
                            }

                            meta_groups.entry(id).or_default().push(name);
                        }

                        _ => {
                            meta_groups.entry(id).or_default().push(name);
                        }
                    }
                }
            }
        }
    }

    let target = project.target().or(target_flag).unwrap_or(&host);

    // we know how to analyze the machine code in the ELF file for these targets thus we have more
    // information and need less LLVM-IR hacks
    let target_ = match target {
        "thumbv6m-none-eabi" => Target::Thumbv6m,
        "thumbv7m-none-eabi" | "thumbv7em-none-eabi" | "thumbv7em-none-eabihf" => Target::Thumbv7m,
        _ => Target::Other,
    };

    // extract stack size information
    // the `.o` file doesn't have address information so we just keep the stack usage information
    let mut stack_sizes: HashMap<_, _> = stack_sizes::analyze_object(&obj)?
        .into_iter()
        .map(|(name, stack)| (name.to_owned(), stack))
        .collect();

    // extract list of "live" symbols (symbols that have not been GC-ed by the linker)
    // this time we use the ELF and not the object file
    let mut symbols = stack_sizes::analyze_executable(&elf)?;

    // clear the thumb bit
    if target_.is_thumb() {
        symbols.defined = symbols
            .defined
            .into_iter()
            .map(|(k, v)| (k & !1, v))
            .collect();
    }

    // remove version strings from undefined symbols
    symbols.undefined = symbols
        .undefined
        .into_iter()
        .map(|sym| {
            if let Some(name) = sym.rsplit("@@").nth(1) {
                name
            } else {
                sym
            }
        })
        .collect();

    let mut has_non_rust_symbols = Maybe::new(
        if symbols.undefined.is_empty() {
            false // don't know, actually
        } else {
            true
        },
        !has_call_metadata,
    );

    // we use this set to detect if there are symbols in the final binary that don't come from Rust
    // code. We start by populating it with all the symbols in the executable
    let mut non_rust_symbols = Maybe::new(
        if has_call_metadata && symbols.undefined.is_empty() {
            symbols
                .defined
                .values()
                .flat_map(|f| f.names())
                .cloned()
                .collect()
        } else {
            HashSet::new()
        },
        !has_call_metadata,
    );

    // extract stack usage info from `libcompiler-builtins.rlib`
    let sysroot_nl = String::from_utf8(
        Command::new("rustc")
            .args(&["--print", "sysroot"])
            .output()?
            .stdout,
    )?;
    // remove trailing newline
    let sysroot = Path::new(sysroot_nl.trim_end());
    let libdir = sysroot.join("lib/rustlib").join(target).join("lib");

    for entry in fs::read_dir(libdir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().map(|ext| ext == "rlib").unwrap_or(false)
            && path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(|stem| stem.starts_with("libcompiler_builtins"))
                .unwrap_or(false)
        {
            let mut ar = Archive::new(File::open(path)?);

            let mut buf = vec![];
            while let Some(entry) = ar.next_entry() {
                let mut entry = entry?;
                let header = entry.header();

                if str::from_utf8(header.identifier())
                    .map(|id| id.contains("compiler_builtins") && id.ends_with(".o"))
                    .unwrap_or(false)
                {
                    buf.clear();
                    entry.read_to_end(&mut buf)?;

                    stack_sizes.extend(
                        stack_sizes::analyze_object(&buf)?
                            .into_iter()
                            .map(|(name, stack)| (name.to_owned(), stack)),
                    );

                    if has_call_metadata && !*has_non_rust_symbols {
                        // all symbols defined in compiler-builtins come from Rust code
                        let elf = &ElfFile::new(&buf).map_err(failure::err_msg)?;

                        fn sub<E>(
                            all_symbols: &mut HashSet<&str>,
                            entries: &[E],
                            elf: &ElfFile,
                        ) -> Result<(), failure::Error>
                        where
                            E: Entry,
                        {
                            use xmas_elf::symbol_table::Type;

                            for entry in entries {
                                let name = entry.get_name(elf);
                                let ty = entry.get_type();

                                if ty == Ok(Type::Func)
                                    || (ty == Ok(Type::NoType)
                                        && name
                                            .map(|name| !name.is_empty() && !is_tag(name))
                                            .unwrap_or(false))
                                {
                                    let name = name.map_err(failure::err_msg)?;

                                    all_symbols.remove(name);
                                }
                            }

                            Ok(())
                        }

                        match elf
                            .find_section_by_name(".symtab")
                            .ok_or_else(|| failure::err_msg("`.symtab` section not found"))?
                            .get_data(elf)
                        {
                            Ok(SectionData::SymbolTable32(entries)) => {
                                sub(&mut *non_rust_symbols, entries, elf)?
                            }

                            Ok(SectionData::SymbolTable64(entries)) => {
                                sub(&mut *non_rust_symbols, entries, elf)?
                            }

                            _ => bail!("malformed .symtab section"),
                        }
                    }
                }
            }
        }
    }

    let mut g = DiGraph::<Node, ()>::new();
    let mut indices = BTreeMap::<Cow<str>, _>::new();

    let mut indirects = Maybe::new(HashMap::<_, Indirect>::new(), has_call_metadata);
    let mut dynamics = Maybe::new(HashMap::<_, Dynamic>::new(), has_call_metadata);
    let mut meta_callers = Maybe::new(HashMap::<_, HashSet<_>>::new(), !has_call_metadata);
    // functions that could be called by `ArgumentV1.formatter`
    let mut formatter_callees = Maybe::new(HashSet::new(), has_call_metadata);
    let mut formatter_callers_ = Maybe::new(HashSet::new(), !has_call_metadata);

    // Some functions may be aliased; we map aliases to a single name. For example, if `foo`,
    // `bar` and `baz` all have the same address then this maps contains: `foo -> foo`, `bar -> foo`
    // and `baz -> foo`.
    let mut aliases = HashMap::new();
    // whether a symbol name is ambiguous after removing the hash
    let mut ambiguous = HashMap::<String, u32>::new();

    // we do a first pass over all the definitions to collect methods in `impl Trait for Type`
    let mut default_methods = Maybe::new(HashSet::new(), has_call_metadata);
    if !has_call_metadata {
        for name in defines.keys() {
            let demangled = rustc_demangle::demangle(name).to_string();

            // `<crate::module::Type as crate::module::Trait>::method::hdeadbeef`
            if demangled.starts_with("<") {
                if let Some(rhs) = demangled.splitn(2, " as ").nth(1) {
                    // rhs = `crate::module::Trait>::method::hdeadbeef`
                    let mut parts = rhs.splitn(2, ">::");

                    if let (Some(trait_), Some(rhs)) = (parts.next(), parts.next()) {
                        // trait_ = `crate::module::Trait`, rhs = `method::hdeadbeef`

                        if let Some(method) = dehash(rhs) {
                            default_methods.insert(format!("{}::{}", trait_, method));
                        }
                    }
                }
            }
        }
    }

    // add all real nodes
    let mut has_stack_usage_info = false;
    let mut has_untyped_symbols = Maybe::new(false, has_call_metadata);
    let mut addr2name = BTreeMap::new();
    for (address, sym) in &symbols.defined {
        let names = sym.names();

        let canonical_name = if names.len() > 1 {
            // if one of the aliases appears in the `stack_sizes` dictionary, use that
            if let Some(needle) = names.iter().find(|name| stack_sizes.contains_key(&***name)) {
                needle
            } else {
                // otherwise, pick the first name that's not a tag
                names
                    .iter()
                    .filter_map(|&name| if is_tag(name) { None } else { Some(name) })
                    .next()
                    .expect("UNREACHABLE")
            }
        } else {
            names[0]
        };

        for name in names {
            aliases.insert(name, canonical_name);

            // let's remove aliases from `non_rust_symbols`
            if has_call_metadata && *name != canonical_name {
                non_rust_symbols.remove(name);
            }
        }

        let _out = addr2name.insert(address, canonical_name);
        debug_assert!(_out.is_none());

        let mut stack = stack_sizes.get(canonical_name).cloned();
        if stack.is_none() {
            // here we inject some target specific information we got from analyzing
            // `libcompiler_builtins.rlib`

            let ad_hoc = match target {
                "thumbv6m-none-eabi" => match canonical_name {
                    "__aeabi_memcpy" | "__aeabi_memset" | "__aeabi_memclr" | "__aeabi_memclr4"
                    | "__aeabi_f2uiz" => {
                        stack = Some(0);
                        true
                    }

                    "__aeabi_memcpy4" | "__aeabi_memset4" | "__aeabi_f2iz" | "__aeabi_fadd"
                    | "__aeabi_fdiv" | "__aeabi_fmul" | "__aeabi_fsub" => {
                        stack = Some(8);
                        true
                    }

                    "memcmp" | "__aeabi_fcmpgt" | "__aeabi_fcmplt" | "__aeabi_i2f"
                    | "__aeabi_ui2f" => {
                        stack = Some(16);
                        true
                    }

                    "__addsf3" => {
                        stack = Some(32);
                        true
                    }

                    "__divsf3" => {
                        stack = Some(40);
                        true
                    }

                    "__mulsf3" => {
                        stack = Some(48);
                        true
                    }

                    _ => false,
                },

                "thumbv7m-none-eabi" | "thumbv7em-none-eabi" | "thumbv7em-none-eabihf" => {
                    match canonical_name {
                        "__aeabi_memclr" | "__aeabi_memclr4" => {
                            stack = Some(0);
                            true
                        }

                        "__aeabi_memcpy" | "__aeabi_memcpy4" | "memcmp" => {
                            stack = Some(16);
                            true
                        }

                        "__aeabi_memset" | "__aeabi_memset4" => {
                            stack = Some(8);
                            true
                        }

                        // ARMv7-M only below this point
                        "__aeabi_f2iz" | "__aeabi_f2uiz" | "__aeabi_fadd" | "__aeabi_fcmpgt"
                        | "__aeabi_fcmplt" | "__aeabi_fdiv" | "__aeabi_fmul" | "__aeabi_fsub"
                        | "__aeabi_i2f" | "__aeabi_ui2f"
                            if target == "thumbv7m-none-eabi" =>
                        {
                            stack = Some(0);
                            true
                        }

                        "__addsf3" | "__mulsf3" if target == "thumbv7m-none-eabi" => {
                            stack = Some(16);
                            true
                        }

                        "__divsf3" if target == "thumbv7m-none-eabi" => {
                            stack = Some(20);
                            true
                        }

                        _ => false,
                    }
                }

                _ => false,
            };

            if ad_hoc {
                warn!(
                    "ad-hoc: injecting stack usage information for `{}` (last checked: Rust {})",
                    canonical_name, VERS
                );
            } else if !target_.is_thumb() {
                warn!("no stack usage information for `{}`", canonical_name);
            }
        } else {
            has_stack_usage_info = true;
        }

        let demangled = rustc_demangle::demangle(canonical_name).to_string();
        if let Some(dehashed) = dehash(&demangled) {
            *ambiguous.entry(dehashed.to_string()).or_insert(0) += 1;
        }

        let idx = g.add_node(Node(canonical_name, stack, false));
        indices.insert(canonical_name.into(), idx);

        if !has_call_metadata {
            if let Some(def) = names.iter().filter_map(|name| defines.get(name)).next() {
                if sig_is_any_formatter_result(&def.sig) {
                    formatter_callees.insert(idx);
                }

                // trait methods look like `<crate::module::Type as crate::module::Trait>::method::h$hash`
                // default trait methods look like `crate::module::Trait::method::h$hash`
                let is_trait_method = demangled.starts_with("<") && demangled.contains(" as ") || {
                    dehash(&demangled)
                        .map(|path| default_methods.contains(path))
                        .unwrap_or(false)
                };

                let is_object_safe = is_trait_method && {
                    match def.sig.inputs.first().as_ref() {
                        Some(Type::Pointer(ty)) => match **ty {
                            // XXX can the receiver be a *specific* function? (e.g. `fn() {foo}`)
                            Type::Fn(_) => false,

                            _ => true,
                        },
                        _ => false,
                    }
                };

                if is_object_safe {
                    let mut sig = def.sig.clone();

                    // erase the type of the reciver
                    sig.inputs[0] = Type::erased();

                    dynamics.entry(sig).or_default().callees.insert(idx);
                } else {
                    indirects
                        .entry(def.sig.clone())
                        .or_default()
                        .callees
                        .insert(idx);
                }
            } else {
                if let Some(sig) = names
                    .iter()
                    .filter_map(|name| declares.get(name).and_then(|decl| decl.sig.clone()))
                    .next()
                {
                    indirects.entry(sig).or_default().callees.insert(idx);
                } else {
                    // from `compiler-builtins`
                    match canonical_name {
                        "__aeabi_memcpy" | "__aeabi_memcpy4" | "__aeabi_memcpy8" => {
                            // `fn(*mut u8, *const u8, usize)`
                            let sig = FnSig {
                                inputs: vec![
                                    Type::Pointer(Box::new(Type::Integer(8))),
                                    Type::Pointer(Box::new(Type::Integer(8))),
                                    Type::Integer(32), // ARM has 32-bit pointers
                                ],
                                output: None,
                            };
                            indirects.entry(sig).or_default().callees.insert(idx);
                        }

                        "__aeabi_memclr" | "__aeabi_memclr4" | "__aeabi_memclr8" => {
                            // `fn(*mut u8, usize)`
                            let sig = FnSig {
                                inputs: vec![
                                    Type::Pointer(Box::new(Type::Integer(8))),
                                    Type::Integer(32), // ARM has 32-bit pointers
                                ],
                                output: None,
                            };
                            indirects.entry(sig).or_default().callees.insert(idx);
                        }

                        "__aeabi_memset" | "__aeabi_memset4" | "__aeabi_memset8" => {
                            // `fn(*mut u8, usize, i32)`
                            let sig = FnSig {
                                inputs: vec![
                                    Type::Pointer(Box::new(Type::Integer(8))),
                                    Type::Integer(32), // ARM has 32-bit pointers
                                    Type::Integer(32),
                                ],
                                output: None,
                            };
                            indirects.entry(sig).or_default().callees.insert(idx);
                        }

                        "__aeabi_fadd" | "__addsf3" | "__aeabi_fsub" | "__subsf3"
                        | "__aeabi_fdiv" | "__divsf3" | "__aeabi_fmul" | "__mulsf3" => {
                            // `fn(f32, f32) -> f32`
                            let sig = FnSig {
                                inputs: vec![Type::Float, Type::Float],
                                output: Some(Box::new(Type::Float)),
                            };
                            indirects.entry(sig).or_default().callees.insert(idx);
                        }

                        "__aeabi_fcmpgt" | "__aeabi_fcmplt" => {
                            // `fn(f32, f32) -> i32`
                            let sig = FnSig {
                                inputs: vec![Type::Float, Type::Float],
                                output: Some(Box::new(Type::Integer(32))),
                            };
                            indirects.entry(sig).or_default().callees.insert(idx);
                        }

                        "__aeabi_f2uiz" | "__aeabi_f2iz" => {
                            // `fn(f32) -> {i,u}32`
                            let sig = FnSig {
                                inputs: vec![Type::Float],
                                output: Some(Box::new(Type::Integer(32))),
                            };
                            indirects.entry(sig).or_default().callees.insert(idx);
                        }

                        "__aeabi_ui2f" | "__aeabi_i2f" => {
                            // `fn({i,u}32) -> f32`
                            let sig = FnSig {
                                inputs: vec![Type::Integer(32)],
                                output: Some(Box::new(Type::Float)),
                            };
                            indirects.entry(sig).or_default().callees.insert(idx);
                        }

                        "__divmoddi4" | "__udivmoddi4" => {
                            // `fn({i,u}64, {i,u}64, *{i,u}64) -> {i,u}64`
                            let sig = FnSig {
                                inputs: vec![
                                    Type::Integer(64),
                                    Type::Integer(64),
                                    Type::Pointer(Box::new(Type::Integer(64))),
                                ],
                                output: Some(Box::new(Type::Integer(64))),
                            };
                            indirects.entry(sig).or_default().callees.insert(idx);
                        }

                        "__aeabi_uldivmod" | "__aeabi_ldivmod" => {
                            // these subroutines don't use a standard calling convention and are
                            // impossible to call from Rust code (they can be called via `asm!`
                            // though). This case is listed here to suppress the warning below
                        }

                        _ => {
                            *has_untyped_symbols = true;
                            warn!("no type information for `{}`", canonical_name);
                        }
                    }
                }
            }
        }
    }

    // to avoid printing several warnings about the same thing
    let mut asm_seen = HashSet::new();
    let mut llvm_seen = HashSet::new();
    // add edges
    let mut edges: HashMap<_, HashSet<_>> = HashMap::new(); // NodeIdx -> [NodeIdx]
    let mut defined = HashSet::new(); // functions that are `define`-d in the LLVM-IR
    for define in defines.values() {
        let (name, caller, callees_seen) = if let Some(canonical_name) = aliases.get(&define.name) {
            defined.insert(*canonical_name);

            let idx = indices[*canonical_name];
            (canonical_name, idx, edges.entry(idx).or_default())
        } else {
            // this symbol was GC-ed by the linker, skip
            continue;
        };

        for stmt in &define.stmts {
            match stmt {
                Stmt::Asm(expr) => {
                    if !asm_seen.contains(expr) {
                        asm_seen.insert(expr);
                        warn!("assuming that asm!(\"{}\") does *not* use the stack", expr);
                    }
                }

                // this is basically `(mem::transmute<*const u8, fn()>(&__some_symbol))()`
                Stmt::BitcastCall(sym) => {
                    // XXX we have some type information for this call but it's unclear if we should
                    // try harder -- does this ever occur in pure Rust programs?

                    let sym = sym.expect("BUG? unnamed symbol is being invoked");
                    let callee = if let Some(idx) = indices.get(sym) {
                        *idx
                    } else {
                        warn!("no stack information for `{}`", sym);

                        let idx = g.add_node(Node(sym, None, false));
                        indices.insert(Cow::Borrowed(sym), idx);
                        idx
                    };

                    g.add_edge(caller, callee, ());
                }

                Stmt::DirectCall(func) => {
                    match *func {
                        // no-op / debug-info
                        "llvm.dbg.value" => continue,
                        "llvm.dbg.declare" => continue,

                        // no-op / compiler-hint
                        "llvm.assume" => continue,

                        // lowers to a single instruction
                        "llvm.trap" => continue,

                        _ => {}
                    }

                    // no-op / compiler-hint
                    if func.starts_with("llvm.lifetime.start")
                        || func.starts_with("llvm.lifetime.end")
                    {
                        continue;
                    }

                    let mut call = |callee| {
                        if !callees_seen.contains(&callee) {
                            g.add_edge(caller, callee, ());
                            callees_seen.insert(callee);
                        }
                    };

                    if target_.is_thumb() && func.starts_with("llvm.") {
                        // we'll analyze the machine code in the ELF file to figure out what these
                        // lower to
                        continue;
                    }

                    // TODO? consider alignment and `value` argument to only include one edge
                    // TODO? consider the `len` argument to elide the call to `*mem*`
                    if func.starts_with("llvm.memcpy.") {
                        if let Some(callee) = indices.get("memcpy") {
                            call(*callee);
                        }

                        // ARMv7-R and the like use these
                        if let Some(callee) = indices.get("__aeabi_memcpy") {
                            call(*callee);
                        }

                        if let Some(callee) = indices.get("__aeabi_memcpy4") {
                            call(*callee);
                        }

                        continue;
                    }

                    // TODO? consider alignment and `value` argument to only include one edge
                    // TODO? consider the `len` argument to elide the call to `*mem*`
                    if func.starts_with("llvm.memset.") || func.starts_with("llvm.memmove.") {
                        if let Some(callee) = indices.get("memset") {
                            call(*callee);
                        }

                        // ARMv7-R and the like use these
                        if let Some(callee) = indices.get("__aeabi_memset") {
                            call(*callee);
                        }

                        if let Some(callee) = indices.get("__aeabi_memset4") {
                            call(*callee);
                        }

                        if let Some(callee) = indices.get("memclr") {
                            call(*callee);
                        }

                        if let Some(callee) = indices.get("__aeabi_memclr") {
                            call(*callee);
                        }

                        if let Some(callee) = indices.get("__aeabi_memclr4") {
                            call(*callee);
                        }

                        continue;
                    }

                    // XXX unclear whether these produce library calls on some platforms or not
                    if func.starts_with("llvm.bswap.")
                        | func.starts_with("llvm.ctlz.")
                        | func.starts_with("llvm.uadd.with.overflow.")
                        | func.starts_with("llvm.umul.with.overflow.")
                        | func.starts_with("llvm.usub.with.overflow.")
                    {
                        if !llvm_seen.contains(func) {
                            llvm_seen.insert(func);
                            warn!("assuming that `{}` directly lowers to machine code", func);
                        }

                        continue;
                    }

                    assert!(
                        !func.starts_with("llvm."),
                        "BUG: unhandled llvm intrinsic: {}",
                        func
                    );

                    // use canonical name
                    let callee = if let Some(canon) = aliases.get(func) {
                        indices[*canon]
                    } else {
                        assert!(
                            symbols.undefined.contains(func),
                            "BUG: callee `{}` is unknown",
                            func
                        );

                        if let Some(idx) = indices.get(*func) {
                            *idx
                        } else {
                            let idx = g.add_node(Node(*func, None, false));
                            indices.insert((*func).into(), idx);

                            idx
                        }
                    };

                    if !callees_seen.contains(&callee) {
                        callees_seen.insert(callee);
                        g.add_edge(caller, callee, ());
                    }
                }

                Stmt::IndirectCall(sig, metadata) => {
                    if has_call_metadata {
                        let id = metadata
                            .iter()
                            .filter_map(|meta| {
                                if meta.kind == "rust" {
                                    Some(meta.id)
                                } else {
                                    None
                                }
                            })
                            .next()
                            .ok_or_else(|| {
                                format_err!(
                                    "indirect call in `{}` contains no `rust` metadata",
                                    name
                                )
                            })?;

                        match meta_defs.get(&id) {
                            None => bail!(
                                "indirect call in `{}` contains undefined `rust` metadata",
                                name
                            ),

                            Some(MetadataKind::Set(..)) => bail!(
                                "indirect call in `{}` contains a `rust` metadata 'set'",
                                name
                            ),

                            // special case: `fn(Void, Formatter) -> Result` is a pseudo trait object
                            Some(MetadataKind::Fn { .. }) => {
                                if sig_is_void_formatter_result(sig) {
                                    formatter_callers_.insert(caller);
                                    continue;
                                }
                            }

                            _ => {}
                        }

                        meta_callers.entry(id).or_default().insert(caller);
                    } else {
                        if sig
                            .inputs
                            .first()
                            .map(|ty| ty.has_been_erased())
                            .unwrap_or(false)
                        {
                            // dynamic dispatch
                            let dynamic = dynamics.entry(sig.clone()).or_default();

                            dynamic.called = true;
                            dynamic.callers.insert(caller);
                        } else {
                            let indirect = indirects.entry(sig.clone()).or_default();

                            indirect.called = true;
                            indirect.callers.insert(caller);
                        }
                    }
                }

                Stmt::Label | Stmt::Comment | Stmt::Other => {}
            }
        }
    }

    // all symbols defined in the LLVM-IR come from Rust
    if has_call_metadata {
        for symbol in &defined {
            non_rust_symbols.remove(symbol);
        }
    }

    // here we parse the machine code in the ELF file to find out edges that don't appear in the
    // LLVM-IR (e.g. `fadd` operation, `call llvm.umul.with.overflow`, etc.) or are difficult to
    // disambiguate from the LLVM-IR (e.g. does this `llvm.memcpy` lower to a call to
    // `__aebi_memcpy`, a call to `__aebi_memcpy4` or machine instructions?)
    if target_.is_thumb() {
        let elf = ElfFile::new(&elf).map_err(failure::err_msg)?;
        let sect = elf.find_section_by_name(".symtab").expect("UNREACHABLE");
        let mut tags: Vec<_> = match sect.get_data(&elf).unwrap() {
            SectionData::SymbolTable32(entries) => entries
                .iter()
                .filter_map(|entry| {
                    let addr = entry.value() as u32;
                    entry.get_name(&elf).ok().and_then(|name| {
                        if name.starts_with("$d") {
                            Some((addr, Tag::Data))
                        } else if name.starts_with("$t") {
                            Some((addr, Tag::Thumb))
                        } else {
                            None
                        }
                    })
                })
                .collect(),
            _ => unreachable!(),
        };

        tags.sort_by(|a, b| a.0.cmp(&b.0));

        if let Some(sect) = elf.find_section_by_name(".text") {
            let stext = sect.address() as u32;
            let text = sect.raw_data(&elf);

            for (address, sym) in &symbols.defined {
                let address = *address as u32;
                let canonical_name = aliases[&sym.names()[0]];
                let mut size = sym.size() as u32;

                if size == 0 {
                    // try harder at finding out the size of this symbol
                    if let Ok(needle) = tags.binary_search_by(|tag| tag.0.cmp(&address)) {
                        let start = tags[needle];
                        if start.1 == Tag::Thumb {
                            if let Some(end) = tags.get(needle + 1) {
                                if end.1 == Tag::Thumb {
                                    size = end.0 - start.0;
                                }
                            }
                        }
                    }
                }

                let start = (address - stext) as usize;
                let end = start + size as usize;
                let (bls, bs, indirect, modifies_sp, our_stack) = thumb::analyze(
                    &text[start..end],
                    address,
                    target_ == Target::Thumbv7m,
                    &tags,
                );
                let caller = indices[canonical_name];

                // sanity check
                if let Some(stack) = our_stack {
                    assert_eq!(
                        stack != 0,
                        modifies_sp,
                        "BUG: our analysis reported that `{}` both uses {} bytes of stack and \
                         it does{} modify SP",
                        canonical_name,
                        stack,
                        if !modifies_sp { " not" } else { "" }
                    );
                }

                // check the correctness of `modifies_sp` and `our_stack`
                // also override LLVM's results when they appear to be wrong
                if let Local::Exact(ref mut llvm_stack) = g[caller].local {
                    if let Some(stack) = our_stack {
                        if *llvm_stack == 0 && stack != 0 {
                            // this could be a `#[naked]` + `asm!` function or `global_asm!`

                            warn!(
                                "LLVM reported zero stack usage for `{}` but \
                                 our analysis reported {} bytes; overriding LLVM's result",
                                canonical_name, stack
                            );

                            *llvm_stack = stack;
                        } else {
                            // in all other cases our results should match

                            assert_eq!(
                                *llvm_stack, stack,
                                "BUG: LLVM reported that `{}` uses {} bytes of stack but \
                                 this doesn't match our analysis",
                                canonical_name, stack
                            );
                        }
                    }

                    assert_eq!(
                        *llvm_stack != 0,
                        modifies_sp,
                        "BUG: LLVM reported that `{}` uses {} bytes of stack but this doesn't \
                         match our analysis",
                        canonical_name,
                        *llvm_stack
                    );
                } else if let Some(stack) = our_stack {
                    g[caller].local = Local::Exact(stack);
                } else if !modifies_sp {
                    // this happens when the function contains intra-branches and our analysis gives
                    // up (`our_stack == None`)
                    g[caller].local = Local::Exact(0);
                }

                if g[caller].local == Local::Unknown {
                    warn!("no stack usage information for `{}`", canonical_name);
                }

                if !defined.contains(canonical_name) && indirect {
                    // this function performs an indirect function call and we have no type
                    // information to narrow down the list of callees so inject the uncertainty
                    // in the form of a call to an unknown function with unknown stack usage

                    warn!(
                        "`{}` performs an indirect function call and there's \
                         no type information about the operation",
                        canonical_name,
                    );
                    let callee = g.add_node(Node("?", None, false));
                    g.add_edge(caller, callee, ());
                }

                let callees_seen = edges.entry(caller).or_default();
                for offset in bls {
                    let addr = (address as i64 + i64::from(offset)) as u64;
                    // address may be off by one due to the thumb bit being set
                    let name = addr2name
                        .get(&addr)
                        .unwrap_or_else(|| panic!("BUG? no symbol at address {}", addr));

                    let callee = indices[*name];
                    if !callees_seen.contains(&callee) {
                        g.add_edge(caller, callee, ());
                        callees_seen.insert(callee);
                    }
                }

                for offset in bs {
                    let addr = (address as i32 + offset) as u32;

                    if addr >= address && addr < (address + size) {
                        // intra-function B branches are not function calls
                    } else {
                        // address may be off by one due to the thumb bit being set
                        let name = addr2name
                            .get(&(addr as u64))
                            .unwrap_or_else(|| panic!("BUG? no symbol at address {}", addr));

                        let callee = indices[*name];
                        if !callees_seen.contains(&callee) {
                            g.add_edge(caller, callee, ());
                            callees_seen.insert(callee);
                        }
                    }
                }
            }
        } else {
            error!(".text section not found")
        }
    }

    // add fictitious nodes for indirect function calls
    if has_call_metadata {
        if !*has_non_rust_symbols && non_rust_symbols.is_empty() {
            *has_non_rust_symbols = true;

            warn!(
                "the program contains untyped, external symbols (e.g. linked in from binary blobs); \
                 function pointer calls can not be bounded"
            );
        }

        if !formatter_callers_.is_empty() {
            let name = "fn(&fmt::Void, &mut fmt::Formatter) -> fmt::Result";
            let call = g.add_node(Node(name, Some(0), true));

            for caller in formatter_callers_.into_iter() {
                g.add_edge(caller, call, ());
            }

            if formatter_callees_.is_empty() {
                error!("BUG? no callees for `{}`", name);
            } else {
                for callee in formatter_callees_.into_iter() {
                    let callee = indices[*callee];
                    g.add_edge(call, callee, ());
                }
            }
        }

        for (id, callers) in meta_callers.into_iter() {
            match meta_defs[&id] {
                MetadataKind::Fn { sig: name } => {
                    let call = g.add_node(Node(name, Some(0), true));

                    for caller in callers {
                        g.add_edge(caller, call, ());
                    }

                    if let Some(callees) = meta_groups.get(&id) {
                        for callee in callees {
                            let callee = indices[**callee];
                            g.add_edge(call, callee, ());
                        }

                        if *has_non_rust_symbols {
                            let unknown = g.add_node(Node("?", None, false));
                            g.add_edge(call, unknown, ());
                        }
                    } else {
                        error!("BUG? no callees for `{}`", name);

                        let unknown = g.add_node(Node("?", None, false));
                        g.add_edge(call, unknown, ());
                    }
                }

                MetadataKind::Dyn { trait_, method } => {
                    let name = format!("(dyn {}).{}", trait_, method);

                    let call = g.add_node(Node(name.clone(), Some(0), true));

                    for caller in callers {
                        g.add_edge(caller, call, ());
                    }

                    if let Some(callees) = meta_groups.get(&id) {
                        for callee in callees {
                            let callee = indices[**callee];
                            g.add_edge(call, callee, ());
                        }
                    } else {
                        error!("BUG? no callees for `{}`", name);

                        let unknown = g.add_node(Node("?", None, false));
                        g.add_edge(call, unknown, ());
                    }
                }

                MetadataKind::Drop { trait_ } => {
                    let name = format!("drop(dyn {})", trait_);

                    let call = g.add_node(Node(name.clone(), Some(0), true));

                    for caller in callers {
                        g.add_edge(caller, call, ());
                    }

                    if let Some(callees) = meta_groups.get(&id) {
                        for callee in callees {
                            let callee = indices[**callee];
                            g.add_edge(call, callee, ());
                        }
                    } else {
                        error!("BUG? no callees for `{}`", name);

                        let unknown = g.add_node(Node("?", None, false));
                        g.add_edge(call, unknown, ());
                    }
                }

                _ => unreachable!(),
            }
        }
    } else {
        if *has_untyped_symbols {
            warn!(
                "the program contains untyped, external symbols (e.g. linked in from binary blobs); \
                 function pointer calls can not be bounded"
            );
        }

        // this is a bit weird but for some reason `ArgumentV1.formatter` sometimes lowers to
        // different LLVM types. In theory it should always be: `i1 (*%fmt::Void,
        // *&core::fmt::Formatter)*` but sometimes the type of the first argument is `%fmt::Void`,
        // sometimes it's `%core::fmt::Void`, sometimes is `%core::fmt::Void.12` and on occasion
        // it's even `%SomeRandomType`
        // To cope with this weird fact the following piece of code will try to find the right LLVM
        // type.
        let all_maybe_void = indirects
            .keys()
            .filter_map(|sig| match (&sig.inputs[..], sig.output.as_ref()) {
                ([Type::Pointer(receiver), Type::Pointer(formatter)], Some(output))
                    if **formatter == Type::Alias("core::fmt::Formatter")
                        && **output == Type::Integer(1) =>
                {
                    if let Type::Alias(receiver) = **receiver {
                        Some(receiver)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        let one_true_void = if all_maybe_void.contains(&"fmt::Void") {
            Some("fmt::Void")
        } else {
            all_maybe_void
                .iter()
                .filter_map(|maybe_void| {
                    // this could be `core::fmt::Void` or `core::fmt::Void.12`
                    if maybe_void.starts_with("core::fmt::Void") {
                        Some(*maybe_void)
                    } else {
                        None
                    }
                })
                .next()
                .or_else(|| {
                    if all_maybe_void.len() == 1 {
                        // we got a random type!
                        Some(all_maybe_void[0])
                    } else {
                        None
                    }
                })
        };

        for (mut sig, indirect) in indirects.into_iter() {
            if !indirect.called {
                continue;
            }

            let callees = if let Some(one_true_void) = one_true_void {
                match (&sig.inputs[..], sig.output.as_ref()) {
                    // special case: this is `ArgumentV1.formatter` a pseudo trait object
                    ([Type::Pointer(void), Type::Pointer(fmt)], Some(output))
                        if **void == Type::Alias(one_true_void)
                            && **fmt == Type::Alias("core::fmt::Formatter")
                            && **output == Type::Integer(1) =>
                    {
                        if formatter_callees.is_empty() {
                            error!("BUG? no callees for `{}`", sig.to_string());
                        }

                        // canonicalize the signature
                        if one_true_void != "fmt::Void" {
                            sig.inputs[0] = Type::Alias("fmt::Void");
                        }

                        &formatter_callees
                    }

                    _ => &indirect.callees,
                }
            } else {
                &indirect.callees
            };

            let mut name = sig.to_string();
            // append '*' to denote that this is a function pointer
            name.push('*');

            let call = g.add_node(Node(name.clone(), Some(0), true));

            for caller in &indirect.callers {
                g.add_edge(*caller, call, ());
            }

            if *has_untyped_symbols {
                // add an edge between this and a potential extern / untyped symbol
                let extern_sym = g.add_node(Node("?", None, false));
                g.add_edge(call, extern_sym, ());
            } else {
                if callees.is_empty() {
                    error!("BUG? no callees for `{}`", name);
                }
            }

            for callee in callees {
                g.add_edge(call, *callee, ());
            }
        }

        // add fictitious nodes for dynamic dispatch
        for (sig, dynamic) in dynamics.into_iter() {
            if !dynamic.called {
                continue;
            }

            let name = sig.to_string();

            if dynamic.callees.is_empty() {
                error!("BUG? no callees for `{}`", name);
            }

            let call = g.add_node(Node(name, Some(0), true));
            for caller in &dynamic.callers {
                g.add_edge(*caller, call, ());
            }

            for callee in &dynamic.callees {
                g.add_edge(call, *callee, ());
            }
        }
    }

    // filter the call graph
    if let Some(start) = matches.value_of("START") {
        let start = indices.get(start).cloned().or_else(|| {
            let start_ = start.to_owned() + "::h";
            let hits = indices
                .keys()
                .filter_map(|key| {
                    if rustc_demangle::demangle(key)
                        .to_string()
                        .starts_with(&start_)
                    {
                        Some(key)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            if hits.len() > 1 {
                error!("multiple matches for `{}`: {:?}", start, hits);
                None
            } else {
                hits.first().map(|key| indices[*key])
            }
        });

        if let Some(start) = start {
            // create a new graph that only contains nodes reachable from `start`
            let mut g2 = DiGraph::<Node, ()>::new();

            // maps `g`'s `NodeIndex`-es to `g2`'s `NodeIndex`-es
            let mut one2two = BTreeMap::new();

            let mut dfs = Dfs::new(&g, start);
            while let Some(caller1) = dfs.next(&g) {
                let caller2 = if let Some(i2) = one2two.get(&caller1) {
                    *i2
                } else {
                    let i2 = g2.add_node(g[caller1].clone());
                    one2two.insert(caller1, i2);
                    i2
                };

                let mut callees = g.neighbors(caller1).detach();
                while let Some((_, callee1)) = callees.next(&g) {
                    let callee2 = if let Some(i2) = one2two.get(&callee1) {
                        *i2
                    } else {
                        let i2 = g2.add_node(g[callee1].clone());
                        one2two.insert(callee1, i2);
                        i2
                    };

                    g2.add_edge(caller2, callee2, ());
                }
            }

            // replace the old graph
            g = g2;

            // invalidate `indices` to prevent misuse
            indices.clear();
        } else {
            error!("start point not found; the graph will not be filtered")
        }
    }

    let mut cycles = vec![];
    if !has_stack_usage_info {
        error!("The graph has zero stack usage information; skipping max stack usage analysis");
    } else if algo::is_cyclic_directed(&g) {
        let sccs = algo::kosaraju_scc(&g);

        // iterate over SCCs (Strongly Connected Components) in reverse topological order
        for scc in &sccs {
            let first = scc[0];

            let is_a_cycle = scc.len() > 1
                || g.neighbors_directed(first, Direction::Outgoing)
                    .any(|n| n == first);

            if is_a_cycle {
                cycles.push(scc.clone());

                let mut scc_local =
                    max_of(scc.iter().map(|node| g[*node].local.into())).expect("UNREACHABLE");

                // the cumulative stack usage is only exact when all nodes do *not* use the stack
                if let Max::Exact(n) = scc_local {
                    if n != 0 {
                        scc_local = Max::LowerBound(n)
                    }
                }

                let neighbors_max = max_of(scc.iter().flat_map(|inode| {
                    g.neighbors_directed(*inode, Direction::Outgoing)
                        .filter_map(|neighbor| {
                            if scc.contains(&neighbor) {
                                // we only care about the neighbors of the SCC
                                None
                            } else {
                                Some(g[neighbor].max.expect("UNREACHABLE"))
                            }
                        })
                }));

                for inode in scc {
                    let node = &mut g[*inode];
                    if let Some(max) = neighbors_max {
                        node.max = Some(max + scc_local);
                    } else {
                        node.max = Some(scc_local);
                    }
                }
            } else {
                let inode = first;

                let neighbors_max = max_of(
                    g.neighbors_directed(inode, Direction::Outgoing)
                        .map(|neighbor| g[neighbor].max.expect("UNREACHABLE")),
                );

                let node = &mut g[inode];
                if let Some(max) = neighbors_max {
                    node.max = Some(max + node.local);
                } else {
                    node.max = Some(node.local.into());
                }
            }
        }
    } else {
        // compute max stack usage
        let mut topo = Topo::new(Reversed(&g));
        while let Some(node) = topo.next(Reversed(&g)) {
            debug_assert!(g[node].max.is_none());

            let neighbors_max = max_of(
                g.neighbors_directed(node, Direction::Outgoing)
                    .map(|neighbor| g[neighbor].max.expect("UNREACHABLE")),
            );

            if let Some(max) = neighbors_max {
                g[node].max = Some(max + g[node].local);
            } else {
                g[node].max = Some(g[node].local.into());
            }
        }
    }

    // here we try to shorten the name of the symbol if it doesn't result in ambiguity
    for node in g.node_weights_mut() {
        let demangled = rustc_demangle::demangle(&node.name).to_string();

        if let Some(dehashed) = dehash(&demangled) {
            if ambiguous[dehashed] == 1 {
                node.name = Cow::Owned(dehashed.to_owned());
            }
        }
    }

    dot(g, &cycles)?;

    Ok(0)
}

fn dot(g: Graph<Node, ()>, cycles: &[Vec<NodeIndex>]) -> io::Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    writeln!(stdout, "digraph {{")?;
    writeln!(stdout, "    node [fontname={} shape=box]", FONT)?;

    for (i, node) in g.raw_nodes().iter().enumerate() {
        let node = &node.weight;

        write!(stdout, "    {} [label=\"", i,)?;

        let mut escaper = Escaper::new(&mut stdout);
        write!(escaper, "{}", rustc_demangle::demangle(&node.name)).ok();
        escaper.error?;

        if let Some(max) = node.max {
            write!(stdout, "\\nmax {}", max)?;
        }

        write!(stdout, "\\nlocal = {}\"", node.local,)?;

        if node.dashed {
            write!(stdout, " style=dashed")?;
        }

        writeln!(stdout, "]")?;
    }

    for edge in g.raw_edges() {
        writeln!(
            stdout,
            "    {} -> {}",
            edge.source().index(),
            edge.target().index()
        )?;
    }

    for (i, cycle) in cycles.iter().enumerate() {
        writeln!(stdout, "\n    subgraph cluster_{} {{", i)?;
        writeln!(stdout, "        style=dashed")?;
        writeln!(stdout, "        fontname={}", FONT)?;
        writeln!(stdout, "        label=\"SCC{}\"", i)?;

        for node in cycle {
            writeln!(stdout, "        {}", node.index())?;
        }

        writeln!(stdout, "    }}")?;
    }

    writeln!(stdout, "}}")
}

struct Escaper<W>
where
    W: io::Write,
{
    writer: W,
    error: io::Result<()>,
}

impl<W> Escaper<W>
where
    W: io::Write,
{
    fn new(writer: W) -> Self {
        Escaper {
            writer,
            error: Ok(()),
        }
    }
}

impl<W> fmt::Write for Escaper<W>
where
    W: io::Write,
{
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            self.write_char(c)?;
        }

        Ok(())
    }

    fn write_char(&mut self, c: char) -> fmt::Result {
        match (|| -> io::Result<()> {
            match c {
                '"' => write!(self.writer, "\\")?,
                _ => {}
            }

            write!(self.writer, "{}", c)
        })() {
            Err(e) => {
                self.error = Err(e);

                Err(fmt::Error)
            }
            Ok(()) => Ok(()),
        }
    }
}

#[derive(Clone)]
struct Node<'a> {
    name: Cow<'a, str>,
    local: Local,
    max: Option<Max>,
    dashed: bool,
}

#[allow(non_snake_case)]
fn Node<'a, S>(name: S, stack: Option<u64>, dashed: bool) -> Node<'a>
where
    S: Into<Cow<'a, str>>,
{
    Node {
        name: name.into(),
        local: stack.map(Local::Exact).unwrap_or(Local::Unknown),
        max: None,
        dashed,
    }
}

/// Local stack usage
#[derive(Clone, Copy, PartialEq)]
enum Local {
    Exact(u64),
    Unknown,
}

impl fmt::Display for Local {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Local::Exact(n) => write!(f, "{}", n),
            Local::Unknown => f.write_str("?"),
        }
    }
}

impl Into<Max> for Local {
    fn into(self) -> Max {
        match self {
            Local::Exact(n) => Max::Exact(n),
            Local::Unknown => Max::LowerBound(0),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Max {
    Exact(u64),
    LowerBound(u64),
}

impl ops::Add<Local> for Max {
    type Output = Max;

    fn add(self, rhs: Local) -> Max {
        match (self, rhs) {
            (Max::Exact(lhs), Local::Exact(rhs)) => Max::Exact(lhs + rhs),
            (Max::Exact(lhs), Local::Unknown) => Max::LowerBound(lhs),
            (Max::LowerBound(lhs), Local::Exact(rhs)) => Max::LowerBound(lhs + rhs),
            (Max::LowerBound(lhs), Local::Unknown) => Max::LowerBound(lhs),
        }
    }
}

impl ops::Add<Max> for Max {
    type Output = Max;

    fn add(self, rhs: Max) -> Max {
        match (self, rhs) {
            (Max::Exact(lhs), Max::Exact(rhs)) => Max::Exact(lhs + rhs),
            (Max::Exact(lhs), Max::LowerBound(rhs)) => Max::LowerBound(lhs + rhs),
            (Max::LowerBound(lhs), Max::Exact(rhs)) => Max::LowerBound(lhs + rhs),
            (Max::LowerBound(lhs), Max::LowerBound(rhs)) => Max::LowerBound(lhs + rhs),
        }
    }
}

fn max_of(mut iter: impl Iterator<Item = Max>) -> Option<Max> {
    iter.next().map(|first| iter.fold(first, max))
}

fn max(lhs: Max, rhs: Max) -> Max {
    match (lhs, rhs) {
        (Max::Exact(lhs), Max::Exact(rhs)) => Max::Exact(cmp::max(lhs, rhs)),
        (Max::Exact(lhs), Max::LowerBound(rhs)) => Max::LowerBound(cmp::max(lhs, rhs)),
        (Max::LowerBound(lhs), Max::Exact(rhs)) => Max::LowerBound(cmp::max(lhs, rhs)),
        (Max::LowerBound(lhs), Max::LowerBound(rhs)) => Max::LowerBound(cmp::max(lhs, rhs)),
    }
}

impl fmt::Display for Max {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Max::Exact(n) => write!(f, "= {}", n),
            Max::LowerBound(n) => write!(f, ">= {}", n),
        }
    }
}

// used to track indirect function calls (`fn` pointers)
#[derive(Default)]
struct Indirect {
    called: bool,
    callers: HashSet<NodeIndex>,
    callees: HashSet<NodeIndex>,
}

// used to track dynamic dispatch (trait objects)
#[derive(Debug, Default)]
struct Dynamic {
    called: bool,
    callers: HashSet<NodeIndex>,
    callees: HashSet<NodeIndex>,
}

// removes hashes like `::hfc5adc5d79855638`, if present
fn dehash(demangled: &str) -> Option<&str> {
    const HASH_LENGTH: usize = 19;

    let len = demangled.as_bytes().len();
    if len > HASH_LENGTH {
        if demangled
            .get(len - HASH_LENGTH..)
            .map(|hash| hash.starts_with("::h"))
            .unwrap_or(false)
        {
            Some(&demangled[..len - HASH_LENGTH])
        } else {
            None
        }
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Other,
    Thumbv6m,
    Thumbv7m,
}

impl Target {
    fn is_thumb(&self) -> bool {
        match *self {
            Target::Thumbv6m | Target::Thumbv7m => true,
            Target::Other => false,
        }
    }
}
