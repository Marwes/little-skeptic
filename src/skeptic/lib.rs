extern crate bytecount;
#[macro_use]
extern crate error_chain;
extern crate failure;
extern crate glob;
extern crate handlebars;
extern crate pulldown_cmark as cmark;
#[macro_use]
extern crate serde_json;
extern crate tempdir;

use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{self, Error as IoError, Read, Write};
use std::mem;
use std::path::{Path, PathBuf};

use cmark::{Event, Parser, Tag};

/// Returns a list of markdown files under a directory.
///
/// # Usage
///
/// List markdown files of `mdbook` which are under `<project dir>/book` usually:
///
/// ```rust
/// extern crate little_skeptic;
///
/// use little_skeptic::markdown_files_of_directory;
///
/// fn main() {
///     let _ = markdown_files_of_directory("book/");
/// }
/// ```
pub fn markdown_files_of_directory(dir: &str) -> Vec<PathBuf> {
    use glob::{glob_with, MatchOptions};

    let opts = MatchOptions {
        case_sensitive: false,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    let mut out = Vec::new();

    for path in glob_with(&format!("{}/**/*.md", dir), opts)
        .expect("Failed to read glob pattern")
        .filter_map(Result::ok)
    {
        out.push(path.to_str().unwrap().into());
    }

    out
}

/// Generates tests for specified markdown files.
///
/// # Usage
///
/// Generates doc tests for the specified files.
///
/// ```rust,no_run
/// extern crate little_skeptic;
///
/// use little_skeptic::generate_doc_tests;
///
/// fn main() {
///     generate_doc_tests(&["README.md"]);
/// }
/// ```
///
/// Or in case you want to add `mdbook` files:
///
/// ```rust,no_run
/// extern crate little_skeptic;
///
/// use little_skeptic::*;
///
/// fn main() {
///     let mut mdbook_files = markdown_files_of_directory("book/");
///     mdbook_files.push("README.md".into());
///     generate_doc_tests(&mdbook_files);
/// }
/// ```
pub fn generate_doc_tests<T: Clone>(docs: &[T])
where
    T: AsRef<Path>,
{
    Config {
        print_rerun_if_changed: true,
    }
    .generate_doc_tests(docs)
}

#[derive(Default)]
pub struct Config {
    pub print_rerun_if_changed: bool,
}

impl Config {
    pub fn generate_doc_tests<T: Clone>(&self, docs: &[T])
    where
        T: AsRef<Path>,
    {
        // This shortcut is specifically so examples in skeptic's on
        // readme can call this function in non-build.rs contexts, without
        // panicking below.
        if docs.is_empty() {
            return;
        }

        let docs = docs
            .iter()
            .cloned()
            .map(|path| path.as_ref().to_str().unwrap().to_owned())
            .filter(|d| !d.ends_with(".skt.md"))
            .collect::<Vec<_>>();

        if self.print_rerun_if_changed {
            // Inform cargo that it needs to rerun the build script if one of the skeptic files are
            // modified
            for doc in &docs {
                println!("cargo:rerun-if-changed={}", doc);

                let skt = format!("{}.skt.md", doc);
                if Path::new(&skt).exists() {
                    println!("cargo:rerun-if-changed={}", skt);
                }
            }
        }

        let out_dir = env::var("OUT_DIR").unwrap();
        let cargo_manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

        let mut out_file = PathBuf::from(out_dir.clone());
        out_file.push("skeptic-tests.rs");

        let config = InternalConfig {
            root_dir: PathBuf::from(cargo_manifest_dir),
            out_file: out_file,
            docs: docs,
        };

        run(&config);
    }
}

struct InternalConfig {
    root_dir: PathBuf,
    out_file: PathBuf,
    docs: Vec<String>,
}

fn run(config: &InternalConfig) {
    let tests = extract_tests(config).unwrap_or_else(|err| panic!("{}", err));
    emit_tests(config, tests).unwrap_or_else(|err| panic!("{}", err));
}

struct Test {
    name: String,
    text: Vec<String>,
    ignore: bool,
    no_run: bool,
    should_panic: bool,
    template: Option<String>,
}

struct DocTestSuite {
    doc_tests: Vec<DocTest>,
}

struct DocTest {
    path: PathBuf,
    old_template: Option<String>,
    root_template: Option<String>,
    tests: Vec<Test>,
    templates: HashMap<String, String>,
}

fn extract_tests(config: &InternalConfig) -> Result<DocTestSuite, IoError> {
    let mut doc_tests = Vec::new();
    for doc in &config.docs {
        let path = &mut config.root_dir.clone();
        path.push(doc);
        let new_tests = extract_tests_from_file(path)?;
        doc_tests.push(new_tests);
    }
    Ok(DocTestSuite {
        doc_tests: doc_tests,
    })
}

enum Buffer {
    None,
    Code(Vec<String>),
    Header(String),
}

fn extract_tests_from_file(path: &Path) -> Result<DocTest, IoError> {
    let mut file = File::open(path)?;
    let s = &mut String::new();
    file.read_to_string(s)?;

    let file_stem = &sanitize_test_name(path.file_stem().unwrap().to_str().unwrap());

    let (tests, old_template, old_root_template) = extract_tests_from_string(s, file_stem);

    let (templates, root_template) = load_templates(path)?;

    Ok(DocTest {
        path: path.to_owned(),
        root_template: root_template.or(old_root_template),
        old_template,
        tests,
        templates,
    })
}

fn extract_tests_from_string(
    s: &str,
    file_stem: &str,
) -> (Vec<Test>, Option<String>, Option<String>) {
    let mut tests = Vec::new();
    let mut buffer = Buffer::None;
    let mut parser = Parser::new(s);
    let mut section = None;
    let mut code_block_start = 0;
    // Oh this isn't actually a test but a legacy template
    let mut old_template = None;
    let mut root_template = None;

    // In order to call get_offset() on the parser,
    // this loop must not hold an exclusive reference to the parser.
    loop {
        let offset = parser.get_offset();
        let line_number = bytecount::count(&s.as_bytes()[0..offset], b'\n');
        let event = if let Some(event) = parser.next() {
            event
        } else {
            break;
        };
        match event {
            Event::Start(Tag::Header(level)) if level < 3 => {
                buffer = Buffer::Header(String::new());
            }
            Event::End(Tag::Header(level)) if level < 3 => {
                let cur_buffer = mem::replace(&mut buffer, Buffer::None);
                if let Buffer::Header(sect) = cur_buffer {
                    section = Some(sanitize_test_name(&sect));
                }
            }
            Event::Start(Tag::CodeBlock(ref info)) => {
                let code_block_info = parse_code_block_info(info);
                if code_block_info.is_rust {
                    buffer = Buffer::Code(Vec::new());
                }
            }
            Event::Text(text) => {
                if let Buffer::Code(ref mut buf) = buffer {
                    if buf.is_empty() {
                        code_block_start = line_number;
                    }
                    buf.extend(text.lines().map(|s| format!("{}\n", s)));
                } else if let Buffer::Header(ref mut buf) = buffer {
                    buf.push_str(&*text);
                }
            }
            Event::End(Tag::CodeBlock(ref info)) => {
                let code_block_info = parse_code_block_info(info);
                if let Buffer::Code(buf) = mem::replace(&mut buffer, Buffer::None) {
                    if code_block_info.root_template {
                        root_template = Some(buf.into_iter().collect())
                    } else if code_block_info.is_old_template {
                        old_template = Some(buf.into_iter().collect())
                    } else {
                        let name = if let Some(ref section) = section {
                            format!("{}_sect_{}_line_{}", file_stem, section, code_block_start)
                        } else {
                            format!("{}_line_{}", file_stem, code_block_start)
                        };
                        tests.push(Test {
                            name: name,
                            text: buf,
                            ignore: code_block_info.ignore,
                            no_run: code_block_info.no_run,
                            should_panic: code_block_info.should_panic,
                            template: code_block_info.template,
                        });
                    }
                }
            }
            _ => (),
        }
    }
    (tests, old_template, root_template)
}

fn load_templates(path: &Path) -> Result<(HashMap<String, String>, Option<String>), IoError> {
    let file_name = format!(
        "{}.skt.md",
        path.file_name().expect("no file name").to_string_lossy()
    );
    let path = path.with_file_name(&file_name);
    if !path.exists() {
        return Ok((HashMap::new(), None));
    }

    let mut map = HashMap::new();

    let mut file = File::open(path)?;
    let s = &mut String::new();
    file.read_to_string(s)?;
    let parser = Parser::new(s);

    let mut code_buffer = None;
    let mut root_template = None;

    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(ref info)) => {
                let code_block_info = parse_code_block_info(info);
                if code_block_info.is_rust {
                    code_buffer = Some(Vec::new());
                }
            }
            Event::Text(text) => {
                if let Some(ref mut buf) = code_buffer {
                    buf.push(text.to_string());
                }
            }
            Event::End(Tag::CodeBlock(ref info)) => {
                let code_block_info = parse_code_block_info(info);
                if let Some(buf) = code_buffer.take() {
                    if code_block_info.root_template {
                        root_template = Some(buf.into_iter().collect());
                    } else if let Some(t) = code_block_info.template {
                        map.insert(t, buf.into_iter().collect());
                    }
                }
            }
            _ => (),
        }
    }

    Ok((map, root_template))
}

fn sanitize_test_name(s: &str) -> String {
    s.to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii() && ch.is_alphanumeric() {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

fn parse_code_block_info(info: &str) -> CodeBlockInfo {
    // Same as rustdoc
    let tokens = info.split(|c: char| !(c == '_' || c == '-' || c.is_alphanumeric()));

    let mut seen_rust_tags = false;
    let mut seen_other_tags = false;
    let mut info = CodeBlockInfo {
        is_rust: false,
        should_panic: false,
        ignore: false,
        no_run: false,
        is_old_template: false,
        template: None,
        root_template: false,
    };

    for token in tokens {
        match token {
            "" => {}
            "rust" => {
                info.is_rust = true;
                seen_rust_tags = true
            }
            "should_panic" => {
                info.should_panic = true;
                seen_rust_tags = true
            }
            "ignore" => {
                info.ignore = true;
                seen_rust_tags = true
            }
            "no_run" => {
                info.no_run = true;
                seen_rust_tags = true;
            }
            "skeptic-template" => {
                info.is_old_template = true;
                seen_rust_tags = true
            }
            "skeptic-root-template" => {
                info.root_template = true;
            }
            _ if token.starts_with("skt-") => {
                info.template = Some(token[4..].to_string());
                seen_rust_tags = true;
            }
            _ => seen_other_tags = true,
        }
    }

    info.is_rust &= !seen_other_tags || seen_rust_tags;

    info
}

struct CodeBlockInfo {
    is_rust: bool,
    should_panic: bool,
    ignore: bool,
    no_run: bool,
    is_old_template: bool,
    template: Option<String>,
    root_template: bool,
}

fn emit_tests(config: &InternalConfig, suite: DocTestSuite) -> Result<(), failure::Error> {
    let mut out = String::new();

    for doc_test in suite.doc_tests {
        if let Some(ref root_template) = doc_test.root_template {
            out.push_str(root_template);
        }

        for test in &doc_test.tests {
            let test_string = {
                if let Some(ref t) = test.template {
                    let template = doc_test.templates.get(t).expect(&format!(
                        "template {} not found for {}",
                        t,
                        doc_test.path.display()
                    ));
                    create_test_runner(config, &Some(template.to_string()), test)?
                } else {
                    create_test_runner(config, &doc_test.old_template, test)?
                }
            };
            out.push_str(&test_string);
        }
    }
    Ok(write_if_contents_changed(&config.out_file, &out)?)
}

/// Just like Rustdoc, ignore a "#" sign at the beginning of a line of code.
/// These are commonly an indication to omit the line from user-facing
/// documentation but include it for the purpose of playground links or skeptic
/// testing.
fn clean_omitted_line(line: &str) -> &str {
    let trimmed = line.trim_start();

    if trimmed.starts_with("# ") {
        &trimmed[2..]
    } else if trimmed.trim_end() == "#" {
        // line consists of single "#" which might not be followed by newline on windows
        &trimmed[1..]
    } else {
        line
    }
}

/// Creates the Rust code that this test will be operating on.
fn create_test_input(lines: &[String]) -> String {
    lines
        .iter()
        .map(|s| clean_omitted_line(s).to_owned())
        .collect()
}

fn create_test_runner(
    _config: &InternalConfig,
    template: &Option<String>,
    test: &Test,
) -> Result<String, failure::Error> {
    use handlebars::Handlebars;

    if test.ignore {
        return Ok("".to_string());
    }

    let template = template.clone().unwrap_or_else(|| String::from("{{test}}"));
    let test_text = create_test_input(&test.text);

    let mut reg = Handlebars::new();
    reg.register_escape_fn(|s| s.into());

    let mut s: Vec<u8> = Vec::new();
    if test.no_run {
        writeln!(s, "#[ignore]")?;
    }
    if test.should_panic {
        writeln!(s, "#[should_panic]")?;
    }

    writeln!(s, "#[test] fn {}() {{", test.name)?;
    writeln!(
        s,
        "{}",
        reg.render_template(&template, &json!({ "test": test_text }))?
    )?;

    writeln!(s, "}}")?;
    writeln!(s, "")?;

    Ok(String::from_utf8(s).unwrap())
}

fn write_if_contents_changed(name: &Path, contents: &str) -> Result<(), IoError> {
    // Can't open in write mode now as that would modify the last changed timestamp of the file
    match File::open(name) {
        Ok(mut file) => {
            let mut current_contents = String::new();
            file.read_to_string(&mut current_contents)?;
            if current_contents == contents {
                // No change avoid writing to avoid updating the timestamp of the file
                return Ok(());
            }
        }
        Err(ref err) if err.kind() == io::ErrorKind::NotFound => (),
        Err(err) => return Err(err),
    }
    let mut file = File::create(name)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

pub mod rt {
    extern crate cargo_metadata;
    extern crate serde_json;
    extern crate walkdir;

    use std::collections::hash_map::Entry;
    use std::collections::HashMap;
    use std::time::SystemTime;

    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::{self, env};
    use tempdir::TempDir;

    use self::serde_json::Value;
    use self::walkdir::WalkDir;

    use failure::ResultExt as _;

    error_chain! {
        errors { Fingerprint }
        foreign_links {
            Io(std::io::Error);
            Metadata(failure::Compat<cargo_metadata::Error>);
            Json(serde_json::Error);
        }
    }

    #[derive(Clone, Copy)]
    enum CompileType {
        Full,
        Check,
    }

    // An iterator over the root dependencies in a lockfile
    #[derive(Debug)]
    struct LockedDeps {
        dependencies: Vec<String>,
    }

    impl LockedDeps {
        fn from_path<P: AsRef<Path>>(pth: P) -> Result<LockedDeps> {
            let pth = pth.as_ref().join("Cargo.toml");
            let metadata = cargo_metadata::MetadataCommand::new()
                .manifest_path(&pth)
                .exec()
                .compat()?;
            let workspace_members = metadata.workspace_members;
            let deps = metadata
                .resolve
                .ok_or("Missing dependency metadata")?
                .nodes
                .into_iter()
                .filter(|node| workspace_members.contains(&node.id))
                .flat_map(|node| node.dependencies.into_iter())
                .chain(workspace_members.clone())
                .map(|x| x.to_string());

            Ok(LockedDeps {
                dependencies: deps.collect(),
            })
        }
    }

    impl Iterator for LockedDeps {
        type Item = (String, String);

        fn next(&mut self) -> Option<(String, String)> {
            self.dependencies.pop().and_then(|val| {
                let mut it = val.split_whitespace();

                match (it.next(), it.next()) {
                    (Some(name), Some(val)) => {
                        Some((name.replace("-", "_").to_owned(), val.to_owned()))
                    }
                    _ => None,
                }
            })
        }
    }

    #[derive(Debug)]
    struct Fingerprint {
        libname: String,
        version: Option<String>, // version might not be present on path or vcs deps
        rlib: PathBuf,
        mtime: SystemTime,
    }

    fn guess_ext(mut pth: PathBuf, exts: &[&str]) -> Result<PathBuf> {
        for ext in exts {
            pth.set_extension(ext);
            if pth.exists() {
                return Ok(pth);
            }
        }
        Err(ErrorKind::Fingerprint.into())
    }

    impl Fingerprint {
        fn from_path<P: AsRef<Path>>(pth: P) -> Result<Fingerprint> {
            let pth = pth.as_ref();

            let fname = pth
                .file_stem()
                .and_then(OsStr::to_str)
                .ok_or(ErrorKind::Fingerprint)?;

            pth.extension()
                .and_then(|e| if e == "json" { Some(e) } else { None })
                .ok_or(ErrorKind::Fingerprint)?;

            let mut captures = fname.splitn(3, '-');
            captures.next();
            let libname = captures.next().ok_or(ErrorKind::Fingerprint)?;
            let hash = captures.next().ok_or(ErrorKind::Fingerprint)?;

            let mut rlib = PathBuf::from(pth);
            rlib.pop();
            rlib.pop();
            rlib.pop();
            rlib.push(format!("deps/lib{}-{}", libname, hash));
            rlib = guess_ext(rlib, &["rlib", "so", "dylib", "dll"])?;

            let file = File::open(pth)?;
            let mtime = file.metadata()?.modified()?;
            let parsed: Value = serde_json::from_reader(file)?;
            let vers = parsed["local"]["Precalculated"]
                .as_str()
                // fingerprint file sometimes has different form
                .or_else(|| parsed["local"][0]["Precalculated"].as_str())
                .map(|v| v.to_owned());

            Ok(Fingerprint {
                libname: libname.to_owned(),
                version: vers,
                rlib: rlib,
                mtime: mtime,
            })
        }

        fn name(&self) -> String {
            self.libname.clone()
        }

        fn version(&self) -> Option<String> {
            self.version.clone()
        }
    }

    // Retrieve the exact dependencies for a given build by
    // cross-referencing the lockfile with the fingerprint file
    fn get_rlib_dependencies<P: AsRef<Path>>(
        root_dir: P,
        target_dir: P,
    ) -> Result<Vec<Fingerprint>> {
        let root_dir = root_dir.as_ref();
        let target_dir = target_dir.as_ref();
        let lock = LockedDeps::from_path(root_dir).or_else(|_| {
            // could not find Cargo.lock in $CARGO_MAINFEST_DIR
            // try relative to target_dir
            let mut root_dir = PathBuf::from(target_dir);
            root_dir.pop();
            root_dir.pop();
            LockedDeps::from_path(root_dir)
        })?;

        let fingerprint_dir = target_dir.join(".fingerprint/");
        let locked_deps: HashMap<String, String> = lock.collect();
        let mut found_deps: HashMap<String, Fingerprint> = HashMap::new();

        for finger in WalkDir::new(fingerprint_dir)
            .into_iter()
            .filter_map(|v| v.ok())
            .filter_map(|v| Fingerprint::from_path(v.path()).ok())
        {
            if let Some(locked_ver) = locked_deps.get(&finger.name()) {
                // TODO this should be refactored to something more readable
                match (found_deps.entry(finger.name()), finger.version()) {
                    (Entry::Occupied(mut e), Some(ver)) => {
                        // we find better match only if it is exact version match
                        // and has fresher build time
                        if *locked_ver == ver && e.get().mtime < finger.mtime {
                            e.insert(finger);
                        }
                    }
                    (Entry::Vacant(e), ver) => {
                        // we see an exact match or unversioned version
                        if ver.unwrap_or_else(|| locked_ver.clone()) == *locked_ver {
                            e.insert(finger);
                        }
                    }
                    _ => (),
                }
            }
        }

        Ok(found_deps
            .into_iter()
            .filter_map(|(_, val)| if val.rlib.exists() { Some(val) } else { None })
            .collect())
    }

    pub fn compile_test(root_dir: &str, out_dir: &str, target_triple: &str, test_text: &str) {
        let rustc = &env::var("RUSTC").unwrap_or_else(|_| String::from("rustc"));
        let outdir = &TempDir::new("rust-skeptic").unwrap();
        let testcase_path = &outdir.path().join("test.rs");
        let binary_path = &outdir.path().join("out.exe");

        write_test_case(testcase_path, test_text);
        compile_test_case(
            testcase_path,
            binary_path,
            rustc,
            root_dir,
            out_dir,
            target_triple,
            CompileType::Check,
        );
    }

    pub fn run_test(root_dir: &str, out_dir: &str, target_triple: &str, test_text: &str) {
        let rustc = &env::var("RUSTC").unwrap_or_else(|_| String::from("rustc"));
        let outdir = &TempDir::new("rust-skeptic").unwrap();
        let testcase_path = &outdir.path().join("test.rs");
        let binary_path = &outdir.path().join("out.exe");

        write_test_case(testcase_path, test_text);
        compile_test_case(
            testcase_path,
            binary_path,
            rustc,
            root_dir,
            out_dir,
            target_triple,
            CompileType::Full,
        );
        run_test_case(binary_path, outdir.path());
    }

    fn write_test_case(path: &Path, test_text: &str) {
        let mut file = File::create(path).unwrap();
        file.write_all(test_text.as_bytes()).unwrap();
    }

    fn compile_test_case(
        in_path: &Path,
        out_path: &Path,
        rustc: &str,
        root_dir: &str,
        out_dir: &str,
        target_triple: &str,
        compile_type: CompileType,
    ) {
        // OK, here's where a bunch of magic happens using assumptions
        // about cargo internals. We are going to use rustc to compile
        // the examples, but to do that we've got to tell it where to
        // look for the rlibs with the -L flag, and what their names
        // are with the --extern flag. This is going to involve
        // parsing fingerprints out of the lockfile and looking them
        // up in the fingerprint file.

        let root_dir = PathBuf::from(root_dir);
        let mut target_dir = PathBuf::from(out_dir);
        target_dir.pop();
        target_dir.pop();
        target_dir.pop();
        let mut deps_dir = target_dir.clone();
        deps_dir.push("deps");

        let mut cmd = Command::new(rustc);
        cmd.arg(in_path)
            .arg("--verbose")
            .arg("--crate-type=bin")
            .arg("-L")
            .arg(&target_dir)
            .arg("-L")
            .arg(&deps_dir)
            .arg("--target")
            .arg(&target_triple);

        for dep in get_rlib_dependencies(root_dir, target_dir).expect("failed to read dependencies")
        {
            cmd.arg("--extern");
            cmd.arg(format!(
                "{}={}",
                dep.libname,
                dep.rlib.to_str().expect("filename not utf8"),
            ));
        }

        match compile_type {
            CompileType::Full => cmd.arg("-o").arg(out_path),
            CompileType::Check => cmd.arg(format!(
                "--emit=dep-info={0}.d,metadata={0}.m",
                out_path.display()
            )),
        };

        interpret_output(cmd);
    }

    fn run_test_case(program_path: &Path, outdir: &Path) {
        let mut cmd = Command::new(program_path);
        cmd.current_dir(outdir);
        interpret_output(cmd);
    }

    fn interpret_output(mut command: Command) {
        let output = command.output().unwrap();
        write!(
            io::stdout(),
            "{}",
            String::from_utf8(output.stdout).unwrap()
        )
        .unwrap();
        write!(
            io::stderr(),
            "{}",
            String::from_utf8(output.stderr).unwrap()
        )
        .unwrap();
        if !output.status.success() {
            panic!("Command failed:\n{:?}", command);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate unindent;

    use self::unindent::unindent;
    use super::*;

    #[test]
    fn test_omitted_lines() {
        let lines = unindent(
            r###"
            # use std::collections::BTreeMap as Map;
            #
            #[allow(dead_code)]
            fn main() {
                let map = Map::new();
                #
                # let _ = map;
            }"###,
        );

        let expected = unindent(
            r###"
            use std::collections::BTreeMap as Map;

            #[allow(dead_code)]
            fn main() {
                let map = Map::new();

            let _ = map;
            }
            "###,
        );

        assert_eq!(create_test_input(&get_lines(lines)), expected);
    }

    #[test]
    fn test_markdown_files_of_directory() {
        let files = vec![
            "../../tests/hashtag-test.md",
            "../../tests/macro-use.md",
            "../../tests/section-names.md",
            "../../tests/should-panic-test.md",
        ];
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        assert_eq!(markdown_files_of_directory("../../tests/"), files);
    }

    #[test]
    fn test_sanitization_of_testnames() {
        assert_eq!(sanitize_test_name("My_Fun"), "my_fun");
        assert_eq!(sanitize_test_name("__my_fun_"), "my_fun");
        assert_eq!(sanitize_test_name("^$@__my@#_fun#$@"), "my_fun");
        assert_eq!(
            sanitize_test_name("my_long__fun___name___with____a_____lot______of_______spaces",),
            "my_long_fun_name_with_a_lot_of_spaces"
        );
        assert_eq!(sanitize_test_name("Löwe 老虎 Léopard"), "l_we_l_opard");
    }

    #[test]
    fn line_numbers_displayed_are_for_the_beginning_of_each_code_block() {
        let lines = unindent(
            r###"
            Rust code that should panic when running it.

            ```rust,should_panic",/
            fn main() {
                panic!(\"I should panic\");
            }
            ```

            Rust code that should panic when compiling it.

            ```rust,no_run,should_panic",//
            fn add(a: u32, b: u32) -> u32 {
                a + b
            }

            fn main() {
                add(1);
            }
            ```"###,
        );

        let tests =
            extract_tests_from_string(&create_test_input(&get_lines(lines)), &String::from("blah"));

        let test_names: Vec<String> = tests
            .0
            .into_iter()
            .map(|test| get_line_number_from_test_name(test))
            .collect();

        assert_eq!(test_names, vec!["3", "11"]);
    }

    #[test]
    fn line_numbers_displayed_are_for_the_beginning_of_each_section() {
        let lines = unindent(r###"
            ## Test Case  Names   With    weird     spacing       are        generated      without        error.

            ```rust", /
            struct Person<'a>(&'a str);
            fn main() {
              let _ = Person(\"bors\");
            }
            ```

            ## !@#$ Test Cases )(() with {}[] non alphanumeric characters ^$23 characters are \"`#`\" generated correctly @#$@#$  22.

            ```rust", //
            struct Person<'a>(&'a str);
            fn main() {
              let _ = Person(\"bors\");
            }
            ```

            ## Test cases with non ASCII ö_老虎_é characters are generated correctly.

            ```rust",//
            struct Person<'a>(&'a str);
            fn main() {
              let _ = Person(\"bors\");
            }
            ```"###);

        let tests =
            extract_tests_from_string(&create_test_input(&get_lines(lines)), &String::from("blah"));

        let test_names: Vec<String> = tests
            .0
            .into_iter()
            .map(|test| get_line_number_from_test_name(test))
            .collect();

        assert_eq!(test_names, vec!["3", "12", "21"]);
    }

    #[test]
    fn old_template_is_returned_for_old_skeptic_template_format() {
        let lines = unindent(
            r###"
            ```rust,skeptic-template
            ```rust,ignore
            use std::path::PathBuf;

            fn main() {{
                {}
            }}
            ```
            ```
            "###,
        );
        let expected = unindent(
            r###"
            ```rust,ignore
            use std::path::PathBuf;

            fn main() {{
                {}
            }}
            "###,
        );
        let tests =
            extract_tests_from_string(&create_test_input(&get_lines(lines)), &String::from("blah"));
        assert_eq!(tests.1, Some(expected));
    }

    #[test]
    fn old_template_is_not_returned_if_old_skeptic_template_is_not_specified() {
        let lines = unindent(
            r###"
            ```rust", /
            struct Person<'a>(&'a str);
            fn main() {
              let _ = Person(\"bors\");
            }
            ```
            "###,
        );
        let tests =
            extract_tests_from_string(&create_test_input(&get_lines(lines)), &String::from("blah"));
        assert_eq!(tests.1, None);
    }

    fn get_line_number_from_test_name(test: Test) -> String {
        String::from(
            test.name
                .split('_')
                .last()
                .expect("There were no underscores!"),
        )
    }

    fn get_lines(lines: String) -> Vec<String> {
        lines
            .split('\n')
            .map(|string_slice| format!("{}\n", string_slice)) //restore line endings since they are removed by split.
            .collect()
    }
}
