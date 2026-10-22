//! What a command prints, and its exit status. Commands return values; only this module
//! turns them into text or JSON, so every failure has the same shape: the conclusion on
//! the first line, then details, then `hint:` lines.

use serde_json::{Value, json};

pub const OK: u8 = 0;
/// The condition did not hold: exited, timed out, already started, unknown name...
pub const NOT_MET: u8 = 1;
/// The command line is wrong.
pub const USAGE: u8 = 2;
/// devsrv could not do its job: state directory, broken record, the OS did not answer.
pub const SYSTEM: u8 = 3;

/// A result: the human text and the JSON say the same thing. Successful, unless `code`
/// says that a part of it could not be made (a listing with a broken record in it).
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub json: Value,
    pub code: u8,
}

impl Output {
    pub fn new(stdout: impl Into<String>, json: Value) -> Output {
        Output {
            stdout: stdout.into(),
            stderr: String::new(),
            json,
            code: OK,
        }
    }
    pub fn note(mut self, stderr: impl Into<String>) -> Output {
        self.stderr = stderr.into();
        self
    }
    pub fn code(mut self, code: u8) -> Output {
        self.code = code;
        self
    }
}

/// A failure. `message` is the conclusion; `details` are shown as is (a log tail is one
/// line each); `hints` say what to run next.
#[derive(Debug)]
pub struct Failure {
    pub code: u8,
    pub kind: &'static str,
    pub message: String,
    pub details: Vec<String>,
    pub hints: Vec<String>,
    pub data: Value,
}

impl Failure {
    pub fn new(code: u8, kind: &'static str, message: impl Into<String>) -> Failure {
        Failure {
            code,
            kind,
            message: message.into(),
            details: vec![],
            hints: vec![],
            data: Value::Null,
        }
    }
    pub fn usage(message: impl Into<String>) -> Failure {
        Failure::new(USAGE, "usage", message)
    }
    pub fn system(message: impl Into<String>) -> Failure {
        Failure::new(SYSTEM, "system", message)
    }
    pub fn detail(mut self, line: impl Into<String>) -> Failure {
        self.details.push(line.into());
        self
    }
    pub fn hint(mut self, line: impl Into<String>) -> Failure {
        self.hints.push(line.into());
        self
    }
    pub fn data(mut self, data: Value) -> Failure {
        self.data = data;
        self
    }
    pub fn text(&self) -> String {
        let mut s = format!("devsrv: {}\n", self.message);
        for d in &self.details {
            s.push_str(&format!("  {d}\n"));
        }
        for h in &self.hints {
            s.push_str(&format!("  hint: {h}\n"));
        }
        s
    }
    pub fn json(&self) -> Value {
        json!({
            "ok": false,
            "exit_code": self.code,
            "error": self.kind,
            "message": self.message,
            "details": self.details,
            "hints": self.hints,
            "data": self.data,
        })
    }
}

/// Writes the result and returns the exit status.
pub fn emit(result: Result<Output, Failure>, as_json: bool) -> u8 {
    use std::io::Write;
    let (out, err, code) = match (result, as_json) {
        (Ok(o), false) => (o.stdout, o.stderr, o.code),
        (Ok(o), true) => {
            let mut v = o.json;
            if let Value::Object(m) = &mut v {
                m.insert("ok".into(), Value::Bool(o.code == OK));
                if o.code != OK {
                    m.insert("exit_code".into(), o.code.into());
                }
            }
            (format!("{v}\n"), String::new(), o.code)
        }
        (Err(f), false) => (String::new(), f.text(), f.code),
        (Err(f), true) => (format!("{}\n", f.json()), String::new(), f.code),
    };
    // A closed stdout (`| head -1`) is not a failure of the command.
    let _ = std::io::stdout().write_all(out.as_bytes());
    let _ = std::io::stderr().write_all(err.as_bytes());
    code
}
