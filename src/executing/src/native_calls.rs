use libloading::Library;
use parsing::embedded::{EmbeddedRust, RustReturnType};
use planning::calls::{CallHeadRef, CallProjection, CallValueRef};
use reading::row::Array;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tracing::info;

const CALL_ABI_VERSION: &str = "flowlog-call-abi-v1-i32-bool";

type NativeCall = unsafe extern "C" fn(*const i32, usize, *mut i32) -> i32;

/// A loaded content-addressed module. Keeping the `Library` here guarantees
/// that every resolved function pointer remains valid for the dataflow's life.
pub struct NativeCallModule {
    _library: Library,
    functions: HashMap<String, NativeCall>,
    cache_path: PathBuf,
}

impl NativeCallModule {
    pub fn compile_and_load(
        embedded: &EmbeddedRust,
        requested_cache: Option<&Path>,
    ) -> Result<Arc<Self>, String> {
        let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
        let version_output = Command::new(&rustc)
            .arg("--version")
            .output()
            .map_err(|error| format!("cannot run {:?} --version: {error}", rustc))?;
        if !version_output.status.success() {
            return Err(format!(
                "{:?} --version failed: {}",
                rustc,
                String::from_utf8_lossy(&version_output.stderr).trim()
            ));
        }
        let rustc_version = String::from_utf8_lossy(&version_output.stdout);

        let mut hasher = Sha256::new();
        hasher.update(CALL_ABI_VERSION.as_bytes());
        hasher.update([0]);
        hasher.update(rustc_version.as_bytes());
        hasher.update([0]);
        hasher.update(embedded.source().as_bytes());
        let digest = format!("{:x}", hasher.finalize());

        let cache_dir = cache_directory(requested_cache).join(&digest);
        fs::create_dir_all(&cache_dir).map_err(|error| {
            format!(
                "cannot create embedded Rust cache {}: {error}",
                cache_dir.display()
            )
        })?;

        let library_path = cache_dir.join(format!(
            "{}flowlog_call_{}{}",
            env::consts::DLL_PREFIX,
            digest,
            env::consts::DLL_SUFFIX
        ));
        if !library_path.is_file() {
            compile_module(&rustc, embedded, &digest, &cache_dir, &library_path)?;
        }

        let module = unsafe { Self::load(embedded, library_path.clone()) }.or_else(|first_error| {
            // A killed compiler or an externally damaged cache entry must not
            // permanently poison this source hash. Rebuild beside it and
            // atomically replace the generated library.
            compile_module(
                &rustc,
                embedded,
                &digest,
                &cache_dir,
                &library_path,
            )?;
            unsafe { Self::load(embedded, library_path.clone()) }.map_err(|second_error| {
                format!(
                    "cannot load cached embedded Rust module after rebuild: {second_error}\ninitial error: {first_error}"
                )
            })
        })?;

        info!("embedded Rust module ready ({})", library_path.display());
        Ok(Arc::new(module))
    }

    unsafe fn load(embedded: &EmbeddedRust, cache_path: PathBuf) -> Result<Self, String> {
        let library = Library::new(&cache_path)
            .map_err(|error| format!("cannot load {}: {error}", cache_path.display()))?;
        let mut functions = HashMap::with_capacity(embedded.functions().len());
        for (index, function) in embedded.functions().iter().enumerate() {
            let symbol_name = format!("__flowlog_call_{index}");
            let symbol = library
                .get::<NativeCall>(symbol_name.as_bytes())
                .map_err(|error| {
                    format!(
                        "embedded Rust export {:?} is missing symbol {symbol_name:?}: {error}",
                        function.name()
                    )
                })?;
            functions.insert(function.name().to_string(), *symbol);
        }
        Ok(Self {
            _library: library,
            functions,
            cache_path,
        })
    }

    pub fn resolve(self: &Arc<Self>, projection: &CallProjection) -> ResolvedCallProjection {
        let steps = projection
            .steps()
            .iter()
            .map(|step| ResolvedCallStep {
                name: step.function().to_string(),
                function: *self.functions.get(step.function()).unwrap_or_else(|| {
                    panic!(
                        "embedded Rust function {:?} was validated but not loaded",
                        step.function()
                    )
                }),
                arguments: step.arguments().to_vec(),
                output: step.output(),
            })
            .collect();
        ResolvedCallProjection {
            _module: Arc::clone(self),
            steps,
            head: projection.head().to_vec(),
        }
    }

    pub fn cache_path(&self) -> &Path {
        &self.cache_path
    }
}

struct ResolvedCallStep {
    name: String,
    function: NativeCall,
    arguments: Vec<CallValueRef>,
    output: Option<usize>,
}

/// Function names are resolved into pointers once when each worker assembles
/// its map operator. Tuple evaluation performs no symbol or hash lookup.
pub struct ResolvedCallProjection {
    _module: Arc<NativeCallModule>,
    steps: Vec<ResolvedCallStep>,
    head: Vec<CallHeadRef>,
}

impl ResolvedCallProjection {
    pub fn evaluate<A: Array>(&self, input: &A) -> Option<Vec<i32>> {
        let mut results = Vec::new();
        for step in &self.steps {
            let arguments = step
                .arguments
                .iter()
                .map(|argument| match argument {
                    CallValueRef::Input(index) => input.column(*index),
                    CallValueRef::Result(index) => results[*index],
                    CallValueRef::Constant(value) => *value,
                })
                .collect::<Vec<_>>();
            let mut output = 0i32;
            let status =
                unsafe { (step.function)(arguments.as_ptr(), arguments.len(), &mut output) };
            match status {
                0 => {}
                1 => panic!("embedded Rust function {:?} panicked", step.name),
                other => panic!(
                    "embedded Rust function {:?} returned invalid ABI status {other}",
                    step.name
                ),
            }

            if let Some(index) = step.output {
                assert_eq!(
                    index,
                    results.len(),
                    "embedded call result indices must be dense"
                );
                results.push(output);
            } else {
                match output {
                    0 => return None,
                    1 => {}
                    other => panic!(
                        "bool-returning embedded Rust function {:?} produced {other}",
                        step.name
                    ),
                }
            }
        }

        Some(
            self.head
                .iter()
                .map(|field| match field {
                    CallHeadRef::Input(index) => input.column(*index),
                    CallHeadRef::Result(index) => results[*index],
                })
                .collect(),
        )
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

fn compile_module(
    rustc: &OsString,
    embedded: &EmbeddedRust,
    digest: &str,
    cache_dir: &Path,
    library_path: &Path,
) -> Result<(), String> {
    let generated = render_module(embedded, digest);
    let unique = format!("{}.{}", std::process::id(), digest);
    let source_path = cache_dir.join(format!("module.{unique}.rs"));
    let temporary_library = cache_dir.join(format!("library.{unique}.tmp"));
    fs::write(&source_path, generated).map_err(|error| {
        format!(
            "cannot write generated embedded Rust source {}: {error}",
            source_path.display()
        )
    })?;

    let output = Command::new(rustc)
        .arg("--edition=2021")
        .arg("--crate-type=cdylib")
        .arg("-C")
        .arg("opt-level=3")
        .arg("-C")
        .arg("panic=unwind")
        .arg("--crate-name")
        .arg(format!("flowlog_call_{}", &digest[..16]))
        .arg(&source_path)
        .arg("-o")
        .arg(&temporary_library)
        .output()
        .map_err(|error| format!("cannot run {:?} for embedded Rust: {error}", rustc))?;

    if !output.status.success() {
        return Err(format!(
            "embedded Rust compilation failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    fs::rename(&temporary_library, library_path).map_err(|error| {
        format!(
            "cannot publish embedded Rust library {}: {error}",
            library_path.display()
        )
    })?;
    let stable_source = cache_dir.join("module.rs");
    let _ = fs::rename(&source_path, stable_source);
    Ok(())
}

fn render_module(embedded: &EmbeddedRust, digest: &str) -> String {
    let module_name = format!("__flowlog_generated_abi_{}", &digest[..16]);
    let wrappers = embedded
        .functions()
        .iter()
        .enumerate()
        .map(|(index, function)| {
            let arguments = (0..function.arity())
                .map(|argument| format!("arguments[{argument}]"))
                .collect::<Vec<_>>()
                .join(", ");
            let result = match function.return_type() {
                RustReturnType::I32 => format!(
                    "let value: i32 = super::{}({arguments}); output.write(value);",
                    function.name()
                ),
                RustReturnType::Bool => format!(
                    "let value: bool = super::{}({arguments}); output.write(i32::from(value));",
                    function.name()
                ),
            };
            format!(
                r#"
    #[export_name = "__flowlog_call_{index}"]
    pub unsafe extern "C" fn call_{index}(
        arguments: *const i32,
        argument_count: usize,
        output: *mut i32,
    ) -> i32 {{
        if argument_count != {arity}
            || (argument_count != 0 && arguments.is_null())
            || output.is_null()
        {{
            return 2;
        }}
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {{
            let arguments = std::slice::from_raw_parts(arguments, argument_count);
            {result}
        }}));
        if outcome.is_ok() {{ 0 }} else {{ 1 }}
    }}
"#,
                arity = function.arity(),
            )
        })
        .collect::<String>();

    format!(
        "{}\nmod {} {{\n{}\n}}\n",
        embedded.source(),
        module_name,
        wrappers
    )
}
