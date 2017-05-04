// from https://github.com/tormol/clap-rs/blob/env_splitter/src/app/env_splitter.rs

#[cfg(windows)]
use std::ffi::{OsStr, OsString};
#[cfg(windows)]
use ::osstringext::OsStrExt3;
#[cfg(not(windows))]
use std::os::unix::ffi::OsStrExt;

pub struct SplitArgs {
    args: Vec<u8>,
    pos: usize,
    allow_escape: bool,
}
impl SplitArgs {
    pub fn new(args: OsString, allow_escape: bool) -> Self {
        SplitArgs {
            args: args.as_bytes().to_vec(),
            pos: 0,
            allow_escape: allow_escape,
        }
    }
}
enum State {Normal,SingleQ,DoubleQ,Escape,DQEscape}
impl Iterator for SplitArgs {
    type Item = OsString;
    fn next(&mut self) -> Option<Self::Item> {
        self.pos += self.args[self.pos..]
                        .iter()
                        .take_while(|&b| *b==b' ' || *b==b'\t' || *b==b'\n' || *b==b'\r')
                        .count();
        if self.pos == self.args.len() {
            return None;
        }

        use self::State::*;
        let mut state = Normal;
        let mut arg = Vec::new();
        loop {
            let b = match self.args.get(self.pos) {
                Some(b) => *b,
                None => {// no more bytes
                    match state {// flush state
                        Escape    =>  arg.push(b'\\'),
                        DQEscape  =>  arg.push(b'\\'),
                        _         =>  ()
                    }
                    break;
                }
            };
            self.pos += 1;
            state = match (state, b) {
                (Normal,   b' ')   =>  break,
                (Normal,   b'\t')  =>  break,
                (Normal,   b'\n')  =>  break,
                (Normal,   b'\'')  =>                                 SingleQ ,
                (Normal,   b'"')   =>                                 DoubleQ ,
                (Normal,   b'\\') if self.allow_escape =>             Escape ,
                (Normal,   _)      =>  {                 arg.push(b); Normal },
                (Escape,   _)      =>  {                 arg.push(b); Normal },
                (SingleQ,  b'\'')  =>                                 Normal  ,
                (SingleQ,  _)      =>  {                 arg.push(b); SingleQ},
                (DoubleQ,  b'"')   =>                                 Normal  ,
                (DoubleQ,  b'\\') if self.allow_escape =>             DQEscape,
                (DoubleQ,  _)      =>  {                 arg.push(b); DoubleQ},
                (DQEscape, b'\n')  =>  {                 arg.push(b); DoubleQ },
                (DQEscape, b'\\')  =>  {                 arg.push(b); DoubleQ },
                (DQEscape, b'"')   =>  {                 arg.push(b); DoubleQ },
                (DQEscape, _)      =>  {arg.push(b'\\'); arg.push(b); DoubleQ },
            };
        }
        // strip trailing '\r' in case of '\r\n'
        if arg.last() == Some(&b'\r') {
            arg.pop();
        }
        Some(OsString::from(unsafe{ String::from_utf8_unchecked(arg) }))
    }
}

