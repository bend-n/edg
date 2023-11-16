//! Compile-time expression evaluation.
//! This crate is inspired by [Zig's `comptime`](https://ziglang.org/documentation/master/#comptime).
//!
//! The passed closure will be evaluated at compile time.
//!
//! ### Example
//!
//! ```
//! println!(
//!     "The program was compiled on {}.",
//!     // note how chrono::Utc is transported
//!     edg::r! { || -> chrono::DateTime<chrono::Utc> { chrono::Utc::now() } }.format("%Y-%m-%d").to_string()
//! ); // The program was compiled on 2023-11-16.
//! ```
//!
//! ### Limitations
//!
//! - Unlike Zig, `edg::r!` does not have access to the scope in which it is invoked, as
//! the closure in `edg::r!` is run as its own script.
//! - Unfortunately, as `serde` is not const, you cant have `const X: _ = edg::r! { .. }`.
//! - Each block must be compiled sequentially.
//!
//! ### How it works
//!
//! `edg::r!`:
//!
//! - adds serde_json::to_string to your code
//! - creates a file `edg-{hash}.rs`, with your new code, in your target directory
//! - compiles the file with `rustc`
//! - executes the file
//! - emits code to deserialize the json output.
//!
//! #### Predecessor
//!
//! Much of the code is from the [`comptime`](https://crates.io/crates/comptime) crate.

extern crate proc_macro;

use std::{
    collections::hash_map::DefaultHasher,
    fs::OpenOptions,
    hash::{Hash, Hasher},
    io::ErrorKind,
    path::Path,
    process::Command,
};

use proc_macro::TokenStream;
use quote::{quote, ToTokens};
use syn::{ExprClosure, ReturnType};

fn lock(dir: &Path) {
    loop {
        // no create_new stable :(
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join("lock"))
        {
            Ok(_) => return,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                std::hint::spin_loop();
                continue;
            }
            Err(_) => panic!("unable to create lock"),
        }
    }
}

fn unlock(dir: &Path) {
    std::fs::remove_file(dir.join("lock")).expect("unable to unlock");
}

#[proc_macro]
/// Run a closure at compile time.
/// This closure is completely isolated.
/// You may return any data structure that implements [`serde::Serialize`](https://docs.rs/serde/latest/serde/trait.Serialize.html) and [`serde::Deserialize`](https://docs.rs/serde/latest/serde/trait.Deserialize.html).
///
/// ```
/// let rand = edg::r! { || -> i32 {
/// # mod rand { pub fn random() -> i32 { 4 } }
///     rand::random()
/// } };
/// ```
pub fn r(input: TokenStream) -> TokenStream {
    let out_dir = std::env::current_dir().map_or("/tmp".into(), |p| p.join("target"));
    macro_rules! err {
        ($fstr:literal$(,)? $( $arg:expr ),*) => {{
            unlock(&out_dir);
            let compile_error = format!($fstr, $($arg),*);
            return TokenStream::from(quote!(compile_error!(#compile_error)));
        }};
    }
    lock(&out_dir);

    let args: Vec<_> = std::env::args().collect();

    let input = syn::parse_macro_input!(input as ExprClosure);

    let ty = match input.output {
        ReturnType::Default => err!("specify return type of closure"),
        ReturnType::Type(_, t) => t,
    };

    let code = input.body.to_token_stream().to_string();
    let mut hasher = DefaultHasher::new();
    code.hash(&mut hasher);
    let hash = hasher.finish();

    let file = out_dir.join(format!("edg-{hash}.rs"));
    std::fs::write(
        &file,
        format!(
            r#"fn main() {{
                    let res: {} = 
{code}
; // surely nobody will main()
                    let ser = serde_json::to_string(&res).expect("serialization failed");
                    print!("{{ser}}");
                }}"#,
            ty.to_token_stream().to_string()
        ),
    )
    .expect("could not write file");

    let mut rustc = Command::new("rustc");
    rustc.args(filter_rustc_args(&args));
    rustc.args(["--crate-name", "edg_bin"]);
    rustc.args(["--crate-type", "bin"]);
    rustc.args(["--out-dir".as_ref(), out_dir.as_os_str()]);
    rustc.args(merge_externs(&args));
    rustc.arg(file.to_str().unwrap());

    let compile_output = rustc.output().expect("could not invoke rustc");
    if !compile_output.status.success() {
        err!(
            "could not compile comptime expr:\n\n{}\n",
            String::from_utf8(compile_output.stderr).unwrap()
        );
    }
    print!("{}", String::from_utf8(compile_output.stdout).unwrap());
    print!("{}", String::from_utf8(compile_output.stderr).unwrap());

    let extra = args
        .iter()
        .find(|a| a.starts_with("extra-filename="))
        .map(|ef| ef.split('=').nth(1).unwrap())
        .unwrap_or_default();
    let out = out_dir.join(format!("edg_bin{extra}"));

    let comptime_output = Command::new(&out)
        .output()
        .expect("could not invoke edg_bin");

    if !comptime_output.status.success() {
        err!(
            "could not run comptime expr:\n\n{}\n",
            String::from_utf8(comptime_output.stderr).unwrap()
        );
    }

    let comptime_expr = if let Ok(output) = String::from_utf8(comptime_output.stdout) {
        output
    } else {
        err!("comptime expr output was not utf8")
    };

    _ = std::fs::remove_file(file);
    _ = std::fs::remove_file(out);

    unlock(&out_dir);

    quote!(::serde_json::from_str::<#ty>(#comptime_expr).expect(&format!("deser of expr ({}) failed (bug in `Deserialize` impl)", #comptime_expr))).into()
}

fn filter_rustc_args(args: &[String]) -> Vec<&str> {
    let mut rustc_args = Vec::with_capacity(args.len());
    let mut skip = true;
    for arg in args {
        if &**arg == "-" {
            continue;
        }
        if skip {
            skip = false;
            continue;
        }
        if arg == "--crate-type" || arg == "--crate-name" || arg == "--extern" || arg == "-o" {
            skip = true;
        } else if arg.ends_with(".rs")
            || arg == "--test"
            || arg == "rustc"
            || arg.starts_with("--emit")
        {
            continue;
        } else {
            rustc_args.push(&**arg);
        }
    }
    rustc_args
}

fn merge_externs(args: &[String]) -> Vec<&str> {
    let mut found = false;
    let mut ret = vec![];
    for arg in args {
        match &**arg {
            arg if found => {
                found = false;
                ret.push("--extern");
                ret.push(arg);
            }
            "--extern" => found = true,
            _ => continue,
        }
    }
    ret
}
