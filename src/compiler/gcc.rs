// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use ::compiler::{
    Cacheable,
    CompilerArguments,
};
use compiler::c::{CCompilerImpl, CCompilerKind, ParsedArguments};
use log::LogLevel::Trace;
use futures::future::{self, Future};
use futures_cpupool::CpuPool;
use mock_command::{
    CommandCreatorSync,
    RunCommand,
};
use std::collections::HashMap;
use std::io::Read;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process;
use util::{run_input_output, OsStrExt};

use errors::*;

/// A unit struct on which to implement `CCompilerImpl`.
#[derive(Clone, Debug)]
pub struct GCC;

impl CCompilerImpl for GCC {
    fn kind(&self) -> CCompilerKind { CCompilerKind::GCC }
    fn parse_arguments(&self,
                       arguments: &[OsString],
                       cwd: &Path) -> CompilerArguments<ParsedArguments>
    {
        parse_arguments(arguments, cwd, argument_takes_value)
    }

    fn preprocess<T>(&self,
                     creator: &T,
                     executable: &Path,
                     parsed_args: &ParsedArguments,
                     cwd: &Path,
                     env_vars: &[(OsString, OsString)],
                     pool: &CpuPool)
                     -> SFuture<process::Output> where T: CommandCreatorSync
    {
        preprocess(creator, executable, parsed_args, cwd, env_vars, pool)
    }

    fn compile<T>(&self,
                  creator: &T,
                  executable: &Path,
                  preprocessor_result: process::Output,
                  parsed_args: &ParsedArguments,
                  cwd: &Path,
                  env_vars: &[(OsString, OsString)],
                  pool: &CpuPool)
                  -> SFuture<(Cacheable, process::Output)>
        where T: CommandCreatorSync
    {
        compile(creator, executable, preprocessor_result, parsed_args, cwd, env_vars, pool)
    }
}

/// Arguments that take a value. Shared with clang.
pub const ARGS_WITH_VALUE: &'static [&'static str] = &[
    "--param", "-A", "-D", "-F", "-G", "-I", "-L",
    "-U", "-V", "-Xassembler", "-Xlinker",
    "-Xpreprocessor", "-aux-info", "-b", "-idirafter",
    "-iframework", "-imacros", "-imultilib", "-include",
    "-install_name", "-iprefix", "-iquote", "-isysroot",
    "-isystem", "-iwithprefix", "-iwithprefixbefore",
    "-u",
    ];


/// Return true if `arg` is a GCC commandline argument that takes a value.
pub fn argument_takes_value(arg: &str) -> bool {
    ARGS_WITH_VALUE.contains(&arg)
}

/// Parse `arguments`, determining whether it is supported.
///
/// `argument_takes_value` should return `true` when called with
/// a compiler option that takes a value.
///
/// If any of the entries in `arguments` result in a compilation that
/// cannot be cached, return `CompilerArguments::CannotCache`.
/// If the commandline described by `arguments` is not compilation,
/// return `CompilerArguments::NotCompilation`.
/// Otherwise, return `CompilerArguments::Ok(ParsedArguments)`, with
/// the `ParsedArguments` struct containing information parsed from
/// `arguments`.
pub fn parse_arguments<F: Fn(&str) -> bool>(arguments: &[OsString],
                                            cwd: &Path,
                                            argument_takes_value: F)
                                            -> CompilerArguments<ParsedArguments> {
    _parse_arguments(arguments, cwd, &argument_takes_value)
}

fn _parse_arguments(arguments: &[OsString],
                    cwd: &Path,
                    argument_takes_value: &Fn(&str) -> bool) -> CompilerArguments<ParsedArguments> {
    let mut output_arg = None;
    let mut input_arg = None;
    let mut dep_target = None;
    let mut common_args = vec!();
    let mut preprocessor_args = vec!();
    let mut compilation = false;
    let mut split_dwarf = false;
    let mut need_explicit_dep_target = false;
    let mut force_input_type = None;

    // Custom iterator to expand `@` arguments which stand for reading a file
    // and interpreting it as a list of more arguments.
    let mut it = ExpandIncludeFile {
        stack: arguments.iter().rev().map(|a| a.to_owned()).collect(),
        cwd: cwd,
    };
    while let Some(arg) = it.next() {
        if let Some(s) = arg.to_str() {
            let mut handled = true;
            match s {
                "-c" => compilation = true,
                "-o" => output_arg = it.next(),
                "-gsplit-dwarf" => {
                    split_dwarf = true;
                    common_args.push(arg.clone());
                }
                // Arguments that take a value.
                // -MF and -MQ are in this set but are handled separately
                // because they are also preprocessor options.
                a if argument_takes_value(a) => {
                    common_args.push(arg.clone());
                    if let Some(arg_val) = it.next() {
                        common_args.push(arg_val);
                    }
                },
                // if the input type is forced, we're going to record this
                // so that we can give the proper -x option later on.  We
                // also need to give this to the preprocessor.
                "-x" => {
                    let arg_val = it.next().unwrap();
                    // the extension/input_type will handle putting this on
                    // the compile command, so put this into preprocessor_args only
                    preprocessor_args.push(arg.clone());
                    preprocessor_args.push(arg_val.clone());
                    force_input_type = Some(arg_val.to_str().unwrap().to_owned());
                },
                "-MF" |
                "-MQ" => {
                    preprocessor_args.push(arg.clone());
                    if let Some(arg_val) = it.next() {
                        preprocessor_args.push(arg_val);
                    }
                }
                "-MT" => dep_target = it.next(),
                // Can't cache Clang modules.
                "-fcxx-modules" => return CompilerArguments::CannotCache("clang modules"),
                "-fmodules" => return CompilerArguments::CannotCache("clang modules"),
                // Can't cache -fsyntax-only, it doesn't produce any output.
                "-fsyntax-only" => return CompilerArguments::CannotCache("-fsyntax-only"),
                // Can't cache PGO profiled output.
                "-fprofile-use" => return CompilerArguments::CannotCache("pgo"),
                // We already expanded `@` files we could through
                // `ExpandIncludeFile` above, so if one of those arguments now
                // makes it this far we won't understand it.
                v if v.starts_with('@') => return CompilerArguments::CannotCache("@file"),
                "-M" |
                "-MM" |
                "-MD" |
                "-MMD" => {
                    // If one of the above options is on the command line, we'll
                    // need -MT on the preprocessor command line, whether it's
                    // been passed already or not
                    need_explicit_dep_target = true;
                    preprocessor_args.push(arg.clone());
                }
                _ => handled = false,
            }
            if handled {
                continue
            }
        }

        if arg.starts_with("-") && arg.len() > 1 {
            common_args.push(arg);
        } else {
            // Anything else is an input file.
            if input_arg.is_some() || arg.as_os_str() == "-" {
                // Can't cache compilations with multiple inputs
                // or compilation from stdin.
                trace!("multiple input files -- already had file {:?}, new input {:?}", input_arg, arg);
                return CompilerArguments::CannotCache("multiple input files");
            }
            input_arg = Some(arg);
        }
    }

    // We only support compilation.
    if !compilation {
        return CompilerArguments::NotCompilation;
    }
    let (input, extension) = match input_arg {
        Some(i) => {
            // When compiling from the preprocessed output given as stdin, we need
            // to explicitly pass its file type.
            if let Some(input_type) = force_input_type {
                trace!("force_input_type: {}", input_type);
                (i.to_owned(), input_type.clone())
            } else {
                match Path::new(&i).extension().and_then(|e| e.to_str()) {
                    Some(e @ "c") | Some(e @ "cc") | Some(e @ "cpp") | Some(e @ "cxx") | Some(e @ "c++") => (i.to_owned(), e.to_owned()),
                    e => {
                        trace!("Unknown source extension: {}", e.unwrap_or("(None)"));
                        return CompilerArguments::CannotCache("unknown source extension");
                    }
                }
            }
        }
        // We can't cache compilation without an input.
        None => return CompilerArguments::CannotCache("no input file"),
    };
    let mut outputs = HashMap::new();
    match output_arg {
        // We can't cache compilation that doesn't go to a file
        None => return CompilerArguments::CannotCache("no output file"),
        Some(o) => {
            if split_dwarf {
                let dwo = Path::new(&o).with_extension("dwo");
                outputs.insert("dwo", dwo);
            }
            if need_explicit_dep_target {
                preprocessor_args.push("-MT".into());
                preprocessor_args.push(dep_target.unwrap_or(o.clone()));
            }
            outputs.insert("obj", PathBuf::from(o));
        }
    }

    CompilerArguments::Ok(ParsedArguments {
        input: input.into(),
        extension: extension,
        depfile: None,
        outputs: outputs,
        preprocessor_args: preprocessor_args,
        common_args: common_args,
        msvc_show_includes: false,
    })
}

pub fn preprocess<T>(creator: &T,
                     executable: &Path,
                     parsed_args: &ParsedArguments,
                     cwd: &Path,
                     env_vars: &[(OsString, OsString)],
                     _pool: &CpuPool)
                     -> SFuture<process::Output>
    where T: CommandCreatorSync
{
    trace!("preprocess");
    let mut cmd = creator.clone().new_command_sync(executable);
    cmd.arg("-E")
        .args(&parsed_args.preprocessor_args)
        .args(&parsed_args.common_args)
        .arg(&parsed_args.input)
        .env_clear()
        .envs(env_vars.iter().map(|&(ref k, ref v)| (k, v)))
        .current_dir(cwd);
    if log_enabled!(Trace) {
        trace!("preprocess: {:?}", cmd);
    }
    run_input_output(cmd, None)
}

fn compile<T>(creator: &T,
              executable: &Path,
              preprocessor_result: process::Output,
              parsed_args: &ParsedArguments,
              cwd: &Path,
              env_vars: &[(OsString, OsString)],
              _pool: &CpuPool)
              -> SFuture<(Cacheable, process::Output)>
    where T: CommandCreatorSync
{
    trace!("compile - {:?} (extension {})", parsed_args.input, parsed_args.extension);

    let output = match parsed_args.outputs.get("obj") {
        Some(obj) => obj,
        None => {
            return future::err("Missing object file output".into()).boxed()
        }
    };

    let mut cmd = creator.clone().new_command_sync(executable);
    cmd.args(&["-c", "-x"])
        .arg(match parsed_args.extension.as_ref() {
            "c" => "cpp-output",
            "c++" | "cc" | "cpp" | "cxx" => "c++-cpp-output",
            e => {
                error!("gcc::compile: Got an unexpected file extension {}", e);
                return future::err("Unexpected file extension".into()).boxed()
            }
        })
        .args(&["-", "-o"]).arg(&output)
        .args(&parsed_args.common_args)
        .env_clear()
        .envs(env_vars.iter().map(|&(ref k, ref v)| (k, v)))
        .current_dir(cwd);
    Box::new(run_input_output(cmd, Some(preprocessor_result.stdout)).map(|output| {
        (Cacheable::Yes, output)
    }))
}

struct ExpandIncludeFile<'a> {
    cwd: &'a Path,
    stack: Vec<OsString>,
}

impl<'a> Iterator for ExpandIncludeFile<'a> {
    type Item = OsString;

    fn next(&mut self) -> Option<OsString> {
        loop {
            let arg = match self.stack.pop() {
                Some(arg) => arg,
                None => return None,
            };
            let file = match arg.split_prefix("@") {
                Some(arg) => self.cwd.join(&arg),
                None => return Some(arg),
            };

            // According to gcc [1], @file means:
            //
            //     Read command-line options from file. The options read are
            //     inserted in place of the original @file option. If file does
            //     not exist, or cannot be read, then the option will be
            //     treated literally, and not removed.
            //
            //     Options in file are separated by whitespace. A
            //     whitespace character may be included in an option by
            //     surrounding the entire option in either single or double
            //     quotes. Any character (including a backslash) may be
            //     included by prefixing the character to be included with
            //     a backslash. The file may itself contain additional
            //     @file options; any such options will be processed
            //     recursively.
            //
            // So here we interpret any I/O errors as "just return this
            // argument". Currently we don't implement handling of arguments
            // with quotes, so if those are encountered we just pass the option
            // through literally anyway.
            //
            // At this time we interpret all `@` arguments above as non
            // cacheable, so if we fail to interpret this we'll just call the
            // compiler anyway.
            //
            // [1]: https://gcc.gnu.org/onlinedocs/gcc/Overall-Options.html#Overall-Options
            let mut contents = String::new();
            let res = File::open(&file).and_then(|mut f| {
                f.read_to_string(&mut contents)
            });
            if let Err(e) = res {
                debug!("failed to read @-file `{}`: {}", file.display(), e);
                return Some(arg)
            }
            if contents.contains('"') || contents.contains('\'') {
                return Some(arg)
            }
            let new_args = contents.split_whitespace().collect::<Vec<_>>();
            self.stack.extend(new_args.iter().rev().map(|s| s.into()));
        }
    }
}

#[cfg(test)]
mod test {
    use std::fs::File;
    use std::io::Write;

    use super::*;
    use ::compiler::*;
    use tempdir::TempDir;

    fn _parse_arguments(arguments: &[String]) -> CompilerArguments<ParsedArguments> {
        let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
        parse_arguments(&args, ".".as_ref(), argument_takes_value)
    }

    #[test]
    fn test_parse_arguments_simple() {
        let args = stringvec!["-c", "foo.c", "-o", "foo.o"];
        let ParsedArguments {
            input,
            extension,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
        } = match _parse_arguments(&args) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.c"), input.to_str());
        assert_eq!("c", extension);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        //TODO: fix assert_map_contains to assert no extra keys!
        assert_eq!(1, outputs.len());
        assert!(preprocessor_args.is_empty());
        assert!(common_args.is_empty());
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_parse_arguments_split_dwarf() {
        let args = stringvec!["-gsplit-dwarf", "-c", "foo.cpp", "-o", "foo.o"];
        let ParsedArguments {
            input,
            extension,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
        } = match _parse_arguments(&args) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.cpp"), input.to_str());
        assert_eq!("cpp", extension);
        assert_map_contains!(outputs,
                             ("obj", PathBuf::from("foo.o")),
                             ("dwo", PathBuf::from("foo.dwo")));
        //TODO: fix assert_map_contains to assert no extra keys!
        assert_eq!(2, outputs.len());
        assert!(preprocessor_args.is_empty());
        assert_eq!(ovec!["-gsplit-dwarf"], common_args);
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_parse_arguments_extra() {
        let args = stringvec!["-c", "foo.cc", "-fabc", "-o", "foo.o", "-mxyz"];
        let ParsedArguments {
            input,
            extension,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
        } = match _parse_arguments(&args) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.cc"), input.to_str());
        assert_eq!("cc", extension);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        //TODO: fix assert_map_contains to assert no extra keys!
        assert_eq!(1, outputs.len());
        assert!(preprocessor_args.is_empty());
        assert_eq!(ovec!["-fabc", "-mxyz"], common_args);
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_parse_arguments_values() {
        let args = stringvec!["-c", "foo.cxx", "-fabc", "-I", "include", "-o", "foo.o", "-include", "file"];
        let ParsedArguments {
            input,
            extension,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
        } = match _parse_arguments(&args) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.cxx"), input.to_str());
        assert_eq!("cxx", extension);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        //TODO: fix assert_map_contains to assert no extra keys!
        assert_eq!(1, outputs.len());
        assert!(preprocessor_args.is_empty());
        assert_eq!(ovec!["-fabc", "-I", "include", "-include", "file"], common_args);
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_parse_arguments_preprocessor_args() {
        let args = stringvec!["-c", "foo.c", "-fabc", "-MF", "file", "-o", "foo.o", "-MQ", "abc"];
        let ParsedArguments {
            input,
            extension,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
        } = match _parse_arguments(&args) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.c"), input.to_str());
        assert_eq!("c", extension);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        //TODO: fix assert_map_contains to assert no extra keys!
        assert_eq!(1, outputs.len());
        assert_eq!(ovec!["-MF", "file", "-MQ", "abc"], preprocessor_args);
        assert_eq!(ovec!["-fabc"], common_args);
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_parse_arguments_explicit_dep_target() {
        let args = stringvec!["-c", "foo.c", "-MT", "depfile", "-fabc", "-MF", "file", "-o", "foo.o"];
        let ParsedArguments {
            input,
            extension,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
        } = match _parse_arguments(&args) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.c"), input.to_str());
        assert_eq!("c", extension);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        //TODO: fix assert_map_contains to assert no extra keys!
        assert_eq!(1, outputs.len());
        assert_eq!(ovec!["-MF", "file"], preprocessor_args);
        assert_eq!(ovec!["-fabc"], common_args);
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_parse_arguments_explicit_dep_target_needed() {
        let args = stringvec!["-c", "foo.c", "-MT", "depfile", "-fabc", "-MF", "file", "-o", "foo.o", "-MD"];
        let ParsedArguments {
            input,
            extension,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
        } = match _parse_arguments(&args) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.c"), input.to_str());
        assert_eq!("c", extension);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        //TODO: fix assert_map_contains to assert no extra keys!
        assert_eq!(1, outputs.len());
        assert_eq!(ovec!["-MF", "file", "-MD", "-MT", "depfile"], preprocessor_args);
        assert_eq!(ovec!["-fabc"], common_args);
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_parse_arguments_dep_target_needed() {
        let args = stringvec!["-c", "foo.c", "-fabc", "-MF", "file", "-o", "foo.o", "-MD"];
        let ParsedArguments {
            input,
            extension,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
        } = match _parse_arguments(&args) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.c"), input.to_str());
        assert_eq!("c", extension);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        //TODO: fix assert_map_contains to assert no extra keys!
        assert_eq!(1, outputs.len());
        assert_eq!(ovec!["-MF", "file", "-MD", "-MT", "foo.o"], preprocessor_args);
        assert_eq!(ovec!["-fabc"], common_args);
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_parse_arguments_empty_args() {
        assert_eq!(CompilerArguments::NotCompilation,
                   _parse_arguments(&vec!()));
    }

    #[test]
    fn test_parse_arguments_not_compile() {
        assert_eq!(CompilerArguments::NotCompilation,
                   _parse_arguments(&stringvec!["-o", "foo"]));
    }

    #[test]
    fn test_parse_arguments_too_many_inputs() {
        assert_eq!(CompilerArguments::CannotCache("multiple input files"),
                   _parse_arguments(&stringvec!["-c", "foo.c", "-o", "foo.o", "bar.c"]));
    }

    #[test]
    fn test_parse_arguments_clangmodules() {
        assert_eq!(CompilerArguments::CannotCache("clang modules"),
                   _parse_arguments(&stringvec!["-c", "foo.c", "-fcxx-modules", "-o", "foo.o"]));
        assert_eq!(CompilerArguments::CannotCache("clang modules"),
                   _parse_arguments(&stringvec!["-c", "foo.c", "-fmodules", "-o", "foo.o"]));
    }

    #[test]
    fn test_parse_arguments_pgo() {
        assert_eq!(CompilerArguments::CannotCache("pgo"),
                   _parse_arguments(&stringvec!["-c", "foo.c", "-fprofile-use", "-o", "foo.o"]));
    }

    #[test]
    fn test_parse_arguments_response_file() {
        assert_eq!(CompilerArguments::CannotCache("@file"),
                   _parse_arguments(&stringvec!["-c", "foo.c", "@foo", "-o", "foo.o"]));
    }

    #[test]
    fn at_signs() {
        let td = TempDir::new("sccache").unwrap();
        File::create(td.path().join("foo")).unwrap().write_all(b"\
            -c foo.c -o foo.o\
        ").unwrap();
        let arg = format!("@{}", td.path().join("foo").display());
        let ParsedArguments {
            input,
            extension,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
        } = match _parse_arguments(&[arg]) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.c"), input.to_str());
        assert_eq!("c", extension);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        //TODO: fix assert_map_contains to assert no extra keys!
        assert_eq!(1, outputs.len());
        assert!(preprocessor_args.is_empty());
        assert!(common_args.is_empty());
        assert!(!msvc_show_includes);
    }
}
