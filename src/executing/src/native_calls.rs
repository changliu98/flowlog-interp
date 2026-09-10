//! Embedded functions: compiled per block, loaded once, called over cells.
//!
//! A program's `.code rust` blocks are independent compilation units. Each
//! block is compiled by `rustc` into a shared library named by the digest of
//! the block's source, the compiler's version and the call interface version,
//! so a block that did not change is never rebuilt, whatever else in the
//! program did. The library exports one entry point per public function,
//! `__flowlog_call_<name>`, and one `__flowlog_last_panic` that hands back
//! the location and message of the most recent panic on the calling thread.
//!
//! The call interface (`CALL_ABI_VERSION`) is the engine's contract with the
//! generated wrapper, independent of how the engine lays out rows:
//!
//! ```text
//! fn(arguments: *const i64, count: usize, output: *mut i64, context: *const CallContext) -> i32
//! ```
//!
//! Every argument and result is one `i64` cell: a number is its value, a
//! symbol is its id. `CallContext` gives the function two callbacks over the
//! engine's symbol table - intern a text, resolve an id - which is how the
//! generated `Symbol` type reads and makes texts. The status is 0 for a
//! result, 1 for a panic (the message is waiting in `__flowlog_last_panic`),
//! 2 for a misuse of the boundary, and never a value.
//!
//! A panic inside a function is not a crash of the engine. The wrapper
//! catches the unwinding, the engine reads the message and the line, and the
//! evaluation ends with a diagnostic naming the function, the line of its
//! block as the author wrote it, and the rule that called it.

use crate::symbols::SymbolTable;
use libloading::Library;
use parsing::decl::DataType;
use parsing::diagnostic::{Diagnostic, Location, Result};
use parsing::embedded::{EmbeddedBlock, EmbeddedRust, RustReturnType};
use parsing::Val;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tracing::info;

/// The version of the call interface; part of every library's digest.
pub const CALL_ABI_VERSION: &str = "flowlog-call-abi-v3-cells-context";

/// The callbacks a function reaches the engine's symbol table through. The
/// layout is mirrored in the generated wrapper and must not change without
/// bumping `CALL_ABI_VERSION`.
#[repr(C)]
pub struct CallContext {
    data: *const u8,
    intern: unsafe extern "C" fn(*const u8, *const u8, usize, *mut i64) -> i32,
    resolve: unsafe extern "C" fn(*const u8, i64, *mut *const u8, *mut usize) -> i32,
}

unsafe impl Send for CallContext {}
unsafe impl Sync for CallContext {}

unsafe extern "C" fn intern_symbol(
    data: *const u8,
    pointer: *const u8,
    length: usize,
    output: *mut i64,
) -> i32 {
    if data.is_null() || output.is_null() || (length != 0 && pointer.is_null()) {
        return 3;
    }
    let table = &*(data as *const SymbolTable);
    let bytes = std::slice::from_raw_parts(pointer, length);
    let Ok(text) = std::str::from_utf8(bytes) else {
        return 1;
    };
    match table.intern(text) {
        Ok(id) => {
            *output = id;
            0
        }
        Err(_) => 2,
    }
}

unsafe extern "C" fn resolve_symbol(
    data: *const u8,
    id: i64,
    pointer: *mut *const u8,
    length: *mut usize,
) -> i32 {
    if data.is_null() || pointer.is_null() || length.is_null() {
        return 3;
    }
    let table = &*(data as *const SymbolTable);
    match table.resolve_raw(id) {
        Some((text, text_length)) => {
            *pointer = text;
            *length = text_length;
            0
        }
        None => 1,
    }
}

type NativeCall = unsafe extern "C" fn(*const Val, usize, *mut Val, *const CallContext) -> i32;
type LastPanic = unsafe extern "C" fn(*mut u8, usize) -> usize;

/// One exported function, resolved to its entry point.
#[derive(Clone, Copy)]
pub struct LoadedFunction {
    call: NativeCall,
    block: usize,
    arity: usize,
    return_type: RustReturnType,
}

impl LoadedFunction {
    pub fn arity(&self) -> usize {
        self.arity
    }

    pub fn return_type(&self) -> RustReturnType {
        self.return_type
    }
}

struct LoadedBlock {
    _library: Library,
    last_panic: LastPanic,
    first_line: usize,
    path: PathBuf,
    digest: String,
}

/// Every block of one program, compiled and loaded. Keeping the libraries
/// here keeps every entry point valid for as long as a dataflow may call it.
pub struct NativeCallModule {
    blocks: Vec<LoadedBlock>,
    functions: HashMap<String, LoadedFunction>,
    context: Box<CallContext>,
    _symbols: Arc<SymbolTable>,
    program_name: String,
}

impl NativeCallModule {
    /// Compile every block the cache does not hold, load them all.
    pub fn compile_and_load(
        embedded: &EmbeddedRust,
        requested_cache: Option<&Path>,
        symbols: Arc<SymbolTable>,
        program_name: &str,
    ) -> Result<Arc<Self>> {
        let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
        let version_output = Command::new(&rustc).arg("--version").output().map_err(|error| {
            Diagnostic::function(format!(
                "cannot run {rustc:?} --version, and an embedded function needs the Rust \
                 compiler at run time: {error}"
            ))
        })?;
        if !version_output.status.success() {
            return Err(Diagnostic::function(format!(
                "{rustc:?} --version failed: {}",
                String::from_utf8_lossy(&version_output.stderr).trim()
            )));
        }
        let rustc_version = String::from_utf8_lossy(&version_output.stdout).to_string();
        let cache_root = cache_directory(requested_cache);

        let mut blocks = Vec::with_capacity(embedded.blocks().len());
        let mut functions = HashMap::new();
        for (index, block) in embedded.blocks().iter().enumerate() {
            let loaded = load_block(&rustc, &rustc_version, &cache_root, block, program_name)?;
            for function in block.functions() {
                let symbol_name = format!("__flowlog_call_{}", function.name());
                let call = unsafe { loaded._library.get::<NativeCall>(symbol_name.as_bytes()) }
                    .map_err(|error| {
                        Diagnostic::internal(format!(
                            "embedded Rust export {:?} is missing symbol {symbol_name:?} in {}: \
                             {error}",
                            function.name(),
                            loaded.path.display()
                        ))
                        .with_function(function.name())
                    })?;
                functions.insert(
                    function.name().to_string(),
                    LoadedFunction {
                        call: *call,
                        block: index,
                        arity: function.arity(),
                        return_type: function.return_type(),
                    },
                );
            }
            blocks.push(loaded);
        }

        let context = Box::new(CallContext {
            data: Arc::as_ptr(&symbols) as *const u8,
            intern: intern_symbol,
            resolve: resolve_symbol,
        });
        Ok(Arc::new(Self {
            blocks,
            functions,
            context,
            _symbols: symbols,
            program_name: program_name.to_string(),
        }))
    }

    pub fn function(&self, name: &str) -> Option<&LoadedFunction> {
        self.functions.get(name)
    }

    /// The digests of the loaded blocks, in program order: what a cache key
    /// should name when a unit calls into a block.
    pub fn digests(&self) -> Vec<&str> {
        self.blocks.iter().map(|block| block.digest.as_str()).collect()
    }

    pub fn paths(&self) -> Vec<&Path> {
        self.blocks.iter().map(|block| block.path.as_path()).collect()
    }

    /// Call one function over cells. A panic inside it is a diagnostic naming
    /// the function and its line; it never unwinds into the engine.
    pub fn call(&self, name: &str, function: &LoadedFunction, arguments: &[Val]) -> Result<Val> {
        debug_assert_eq!(arguments.len(), function.arity);
        let mut output = Val::default();
        let status = unsafe {
            (function.call)(
                arguments.as_ptr(),
                arguments.len(),
                &mut output,
                &*self.context,
            )
        };
        match status {
            0 => Ok(output),
            1 => Err(self.panic_diagnostic(name, function)),
            2 => Err(Diagnostic::internal(format!(
                "the call boundary of embedded function {name:?} was used incorrectly: the \
                 loaded library and the engine disagree about its arity"
            ))
            .with_function(name)),
            other => Err(Diagnostic::internal(format!(
                "embedded function {name:?} returned call status {other}, which the interface \
                 does not define"
            ))
            .with_function(name)),
        }
    }

    fn panic_diagnostic(&self, name: &str, function: &LoadedFunction) -> Diagnostic {
        let block = &self.blocks[function.block];
        let length = unsafe { (block.last_panic)(std::ptr::null_mut(), 0) };
        let mut buffer = vec![0u8; length];
        if length > 0 {
            unsafe { (block.last_panic)(buffer.as_mut_ptr(), length) };
        }
        let text = String::from_utf8_lossy(&buffer).to_string();
        let mut parts = text.splitn(3, '\u{1f}');
        let line = parts.next().and_then(|part| part.parse::<usize>().ok()).unwrap_or(0);
        let column = parts.next().and_then(|part| part.parse::<usize>().ok()).unwrap_or(0);
        let message = parts.next().unwrap_or("").trim().to_string();
        let message = if message.is_empty() { "panic".to_string() } else { message };
        let mut diagnostic = Diagnostic::function(if line > 0 {
            format!("embedded function {name} panicked at line {line} of its block: {message}")
        } else {
            format!("embedded function {name} panicked: {message}")
        })
        .with_function(name)
        .with_detail(message.clone());
        if line > 0 {
            diagnostic = diagnostic.with_location(Location::new(
                self.program_name.clone(),
                block.first_line + line - 1,
                column,
            ));
        }
        diagnostic
    }
}

fn cache_directory(requested: Option<&Path>) -> PathBuf {
    if let Some(path) = requested {
        return path.to_path_buf();
    }
    if let Some(path) = env::var_os("FLOWLOG_CALL_CACHE").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(path) = env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(path).join("flowlog").join("calls");
    }
    if let Some(path) = env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(path)
            .join(".cache")
            .join("flowlog")
            .join("calls");
    }
    env::temp_dir().join("flowlog-calls")
}

/// The digest a block's library is named by.
pub fn block_digest(source: &str, rustc_version: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CALL_ABI_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(rustc_version.as_bytes());
    hasher.update([0]);
    hasher.update(source.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn load_block(
    rustc: &OsString,
    rustc_version: &str,
    cache_root: &Path,
    block: &EmbeddedBlock,
    program_name: &str,
) -> Result<LoadedBlock> {
    let digest = block_digest(block.source(), rustc_version);
    let cache_dir = cache_root.join(&digest);
    fs::create_dir_all(&cache_dir).map_err(|error| {
        Diagnostic::function(format!(
            "cannot create the embedded Rust cache {}: {error}",
            cache_dir.display()
        ))
    })?;
    let library_path = cache_dir.join(format!(
        "{}flowlog_block_{}{}",
        env::consts::DLL_PREFIX,
        &digest[..16],
        env::consts::DLL_SUFFIX
    ));
    if !library_path.is_file() {
        compile_block(rustc, block, &digest, &cache_dir, &library_path, program_name)?;
    }

    let library = match unsafe { Library::new(&library_path) } {
        Ok(library) => library,
        Err(first_error) => {
            // A killed compiler or an externally damaged cache entry must not
            // permanently poison this digest: rebuild beside it and replace
            // the library atomically.
            compile_block(rustc, block, &digest, &cache_dir, &library_path, program_name)?;
            unsafe { Library::new(&library_path) }.map_err(|second_error| {
                Diagnostic::function(format!(
                    "cannot load the compiled embedded Rust block at {} after rebuilding it: \
                     {second_error} (first attempt: {first_error})",
                    library_path.display()
                ))
            })?
        }
    };
    let last_panic = unsafe { library.get::<LastPanic>(b"__flowlog_last_panic") }
        .map(|symbol| *symbol)
        .map_err(|error| {
            Diagnostic::internal(format!(
                "the compiled embedded Rust block at {} has no panic channel: {error}",
                library_path.display()
            ))
        })?;
    info!("embedded Rust block ready ({})", library_path.display());
    Ok(LoadedBlock {
        _library: library,
        last_panic,
        first_line: block.first_line(),
        path: library_path,
        digest,
    })
}

fn compile_block(
    rustc: &OsString,
    block: &EmbeddedBlock,
    digest: &str,
    cache_dir: &Path,
    library_path: &Path,
    program_name: &str,
) -> Result<()> {
    let generated = render_block(block);
    let unique = format!("{}.{}", std::process::id(), &digest[..16]);
    let source_path = cache_dir.join(format!("block.{unique}.rs"));
    let temporary_library = cache_dir.join(format!("library.{unique}.tmp"));
    fs::write(&source_path, generated).map_err(|error| {
        Diagnostic::function(format!(
            "cannot write the generated embedded Rust source {}: {error}",
            source_path.display()
        ))
    })?;

    let output = Command::new(rustc)
        .arg("--edition=2021")
        .arg("--crate-type=cdylib")
        .arg("-C")
        .arg("opt-level=3")
        .arg("-C")
        .arg("panic=unwind")
        .arg("--crate-name")
        .arg(format!("flowlog_block_{}", &digest[..16]))
        .arg(&source_path)
        .arg("-o")
        .arg(&temporary_library)
        .output()
        .map_err(|error| {
            Diagnostic::function(format!("cannot run {rustc:?} for an embedded Rust block: {error}"))
        })?;

    if !output.status.success() {
        let _ = fs::remove_file(&temporary_library);
        let _ = fs::remove_file(&source_path);
        return Err(compile_diagnostic(
            block,
            &String::from_utf8_lossy(&output.stderr),
            program_name,
        ));
    }

    fs::rename(&temporary_library, library_path).map_err(|error| {
        Diagnostic::function(format!(
            "cannot publish the embedded Rust library {}: {error}",
            library_path.display()
        ))
    })?;
    let _ = fs::rename(&source_path, cache_dir.join("block.rs"));
    Ok(())
}

/// rustc's report, with its locations said as lines of the block and of the
/// program.
fn compile_diagnostic(block: &EmbeddedBlock, stderr: &str, program_name: &str) -> Diagnostic {
    let block_lines = block.source().lines().count();
    let mut first_location: Option<Location> = None;
    let mut rebased = String::new();
    for line in stderr.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("--> ") {
            // "<path>:<line>:<column>"
            let mut parts = rest.rsplitn(3, ':');
            let column = parts.next().and_then(|part| part.trim().parse::<usize>().ok());
            let source_line = parts.next().and_then(|part| part.parse::<usize>().ok());
            if let (Some(source_line), Some(column)) = (source_line, column) {
                let where_ = if source_line <= block_lines {
                    let program_line = block.first_line() + source_line - 1;
                    if first_location.is_none() {
                        first_location = Some(Location::new(program_name, program_line, column));
                    }
                    format!("line {source_line} of the block (program line {program_line}), column {column}")
                } else {
                    "the generated wrapper around the block".to_string()
                };
                rebased.push_str(&format!("  --> {where_}\n"));
                continue;
            }
        }
        if line.starts_with("error: aborting") {
            continue;
        }
        rebased.push_str(line);
        rebased.push('\n');
    }
    let mut diagnostic = Diagnostic::function(
        "an embedded Rust block does not compile; rustc's report follows with lines counted \
         inside the block",
    )
    .with_detail(rebased.trim_end().to_string());
    if let Some(location) = first_location {
        diagnostic = diagnostic.with_location(location);
    }
    diagnostic
}

/// The source rustc compiles for one block: the block as written, first, so
/// that its lines are the file's lines, then the call interface.
fn render_block(block: &EmbeddedBlock) -> String {
    let wrappers = block
        .functions()
        .iter()
        .map(|function| {
            let arguments = function
                .parameters()
                .iter()
                .enumerate()
                .map(|(index, parameter)| match parameter {
                    DataType::Integer => format!("arguments[{index}]"),
                    DataType::Symbol => format!("super::Symbol(arguments[{index}])"),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let result = match function.return_type() {
                RustReturnType::I64 => format!(
                    "let value: i64 = super::{}({arguments}); output.write(value);",
                    function.name()
                ),
                RustReturnType::Bool => format!(
                    "let value: bool = super::{}({arguments}); output.write(i64::from(value));",
                    function.name()
                ),
                RustReturnType::Symbol => format!(
                    "let value: super::Symbol = super::{}({arguments}); output.write(value.0);",
                    function.name()
                ),
            };
            format!(
                r#"
    #[export_name = "__flowlog_call_{name}"]
    pub unsafe extern "C" fn call_{name}(
        arguments: *const i64,
        argument_count: usize,
        output: *mut i64,
        context: *const Context,
    ) -> i32 {{
        if argument_count != {arity}
            || (argument_count != 0 && arguments.is_null())
            || output.is_null()
            || context.is_null()
        {{
            return 2;
        }}
        install_hook();
        CONTEXT.with(|current| current.set(context));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {{
            let arguments = if argument_count == 0 {{
                &[][..]
            }} else {{
                std::slice::from_raw_parts(arguments, argument_count)
            }};
            {result}
        }}));
        CONTEXT.with(|current| current.set(std::ptr::null()));
        if outcome.is_ok() {{ 0 }} else {{ 1 }}
    }}
"#,
                name = function.name(),
                arity = function.arity(),
            )
        })
        .collect::<String>();

    let mut generated = String::with_capacity(block.source().len() + 4096 + wrappers.len());
    generated.push_str(block.source());
    if !block.source().ends_with('\n') {
        generated.push('\n');
    }
    generated.push_str(SYMBOL_TYPE);
    generated.push_str("\n#[allow(dead_code, non_snake_case, unused_unsafe)]\nmod __flowlog_abi {\n");
    generated.push_str(ABI_MODULE);
    generated.push_str(&wrappers);
    generated.push_str("}\n");
    generated
}

/// The `Symbol` type every block can name in its signatures: a symbol cell
/// that can read its text and make a text into a symbol, through the engine.
const SYMBOL_TYPE: &str = r#"
// ---- generated by FlowLog: the symbol type of the call interface ----
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Symbol(pub i64);

impl Symbol {
    /// The symbol's id: its cell in a row.
    pub fn id(self) -> i64 {
        self.0
    }

    /// The symbol of a text, interning it with the engine.
    pub fn new(text: &str) -> Symbol {
        let context = __flowlog_abi::CONTEXT.with(|current| current.get());
        if context.is_null() {
            panic!("Symbol::new was called outside a FlowLog call");
        }
        let mut id: i64 = 0;
        let status = unsafe {
            ((*context).intern)((*context).data, text.as_ptr(), text.len(), &mut id)
        };
        if status != 0 {
            panic!("the text {:?} could not be made a symbol (status {})", text, status);
        }
        Symbol(id)
    }

    /// The symbol's text.
    pub fn as_str(&self) -> &str {
        let context = __flowlog_abi::CONTEXT.with(|current| current.get());
        if context.is_null() {
            panic!("Symbol::as_str was called outside a FlowLog call");
        }
        let mut pointer: *const u8 = std::ptr::null();
        let mut length: usize = 0;
        let status = unsafe {
            ((*context).resolve)((*context).data, self.0, &mut pointer, &mut length)
        };
        if status != 0 || pointer.is_null() {
            panic!("symbol {} is not a text this engine knows", self.0);
        }
        // The engine's symbol table never frees or moves a text, so the bytes
        // outlive every call that can observe them.
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(pointer, length)) }
    }

    /// The symbol's text, owned.
    pub fn text(&self) -> String {
        self.as_str().to_string()
    }
}

impl std::fmt::Display for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for Symbol {
    fn from(text: &str) -> Symbol {
        Symbol::new(text)
    }
}

impl From<Symbol> for i64 {
    fn from(symbol: Symbol) -> i64 {
        symbol.0
    }
}
"#;

/// The wrapper module: the context the engine hands in, the panic channel,
/// and one entry point per exported function (appended by `render_block`).
const ABI_MODULE: &str = r#"
    use std::cell::{Cell, RefCell};

    #[repr(C)]
    pub struct Context {
        pub data: *const u8,
        pub intern: unsafe extern "C" fn(*const u8, *const u8, usize, *mut i64) -> i32,
        pub resolve: unsafe extern "C" fn(*const u8, i64, *mut *const u8, *mut usize) -> i32,
    }

    thread_local! {
        pub static CONTEXT: Cell<*const Context> = const { Cell::new(std::ptr::null()) };
        static LAST_PANIC: RefCell<String> = RefCell::new(String::new());
    }

    static HOOK: std::sync::Once = std::sync::Once::new();

    pub fn install_hook() {
        HOOK.call_once(|| {
            // This library has its own copy of the standard library, so the
            // hook it installs is its own: the host process's hook is untouched.
            std::panic::set_hook(Box::new(|info| {
                let message = if let Some(text) = info.payload().downcast_ref::<&str>() {
                    text.to_string()
                } else if let Some(text) = info.payload().downcast_ref::<String>() {
                    text.clone()
                } else {
                    String::from("panic")
                };
                let (line, column) = match info.location() {
                    Some(at) => (at.line(), at.column()),
                    None => (0, 0),
                };
                LAST_PANIC.with(|last| {
                    *last.borrow_mut() = format!("{}\u{1f}{}\u{1f}{}", line, column, message)
                });
            }));
        });
    }

    #[export_name = "__flowlog_last_panic"]
    pub unsafe extern "C" fn last_panic(buffer: *mut u8, capacity: usize) -> usize {
        LAST_PANIC.with(|last| {
            let text = last.borrow();
            let bytes = text.as_bytes();
            let count = bytes.len().min(capacity);
            if count > 0 && !buffer.is_null() {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, count);
            }
            bytes.len()
        })
    }
"#;
