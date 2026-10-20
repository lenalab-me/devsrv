//! The help is one file, `help.txt`; nothing in it is written twice. `--help` prints its summary
//! (up to the blank line after "Exit status:"), so that `head -40` cannot cut it. A section is the
//! lines from a heading (a line not indented, after a blank line) to the blank line before the next
//! heading; `devsrv help NAME` prints the usage line and the section that `TOPICS` names.

const TEXT: &str = include_str!("help.txt");

/// A name, the start of the heading of the section that answers it, and the names whose sections
/// matter to it (printed as a last line). A section no name asks for cannot be read: a test fails.
type Topic = (&'static str, &'static str, &'static [&'static str]);

const TOPICS: &[Topic] = &[
    ("start", "start:", &["state", "spelling"]),
    ("ls", "ls:", &[]),
    ("logs", "logs:", &[]),
    ("stop", "stop:", &[]),
    ("who", "who:", &[]),
    ("wait", "wait:", &[]),
    ("state", "state:", &[]),
    ("spelling", "spelling:", &[]),
];

/// The command that a spelling of it means (`run` is `start`), or the name itself.
pub fn canonical(name: &str) -> &str {
    match name {
        "run" => "start",
        "list" | "ps" => "ls",
        "log" => "logs",
        "kill" => "stop",
        other => other,
    }
}

fn names() -> String {
    TOPICS.iter().map(|t| t.0).collect::<Vec<_>>().join(" ")
}

/// The summary: the lines through the "Exit status:" paragraph.
fn summary(lines: &[&'static str]) -> usize {
    let exit = lines
        .iter()
        .position(|l| l.starts_with("Exit status:"))
        .expect("help.txt says the exit status");
    exit + lines[exit..]
        .iter()
        .position(|l| l.trim().is_empty())
        .unwrap_or(lines.len() - exit)
}

/// The sections after the summary, each without its trailing blank lines.
fn sections<'a>(lines: &'a [&'static str]) -> Vec<&'a [&'static str]> {
    let mut starts = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let heading = !l.is_empty() && !l.starts_with(' ') && (i == 0 || lines[i - 1].is_empty());
        if heading {
            starts.push(i);
        }
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).copied().unwrap_or(lines.len());
        while lines[end - 1].trim().is_empty() {
            end -= 1;
        }
        out.push(&lines[s..end]);
    }
    out
}

/// The usage lines of one command: its line in "Usage:" and the lines that continue it.
fn usage(lines: &[&'static str], name: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    let Some(start) = lines.iter().position(|l| l.starts_with("Usage:")) else {
        return out;
    };
    for l in &lines[start + 1..] {
        if !l.starts_with(' ') {
            break;
        }
        match l.strip_prefix("  devsrv ") {
            Some(rest) if !rest.starts_with(' ') => {
                if !out.is_empty() {
                    break;
                }
                if rest.split(' ').next() == Some(name) {
                    out.push(*l);
                }
            }
            _ if !out.is_empty() => out.push(*l),
            _ => {}
        }
    }
    out
}

fn join(lines: &[&str]) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// What `devsrv help [NAME]` and `devsrv NAME --help` print; an unknown name is the message.
pub fn render(name: Option<&str>) -> Result<String, String> {
    let lines: Vec<&'static str> = TEXT.lines().collect();
    let end = summary(&lines);
    let Some(name) = name else {
        let mut s = join(&lines[..end]);
        s.push_str(&format!(
            "\nDetails: devsrv help COMMAND, or devsrv COMMAND --help\n  ({}); devsrv help all: everything.\n",
            names()
        ));
        return Ok(s);
    };
    if name == "all" {
        return Ok(TEXT.to_string());
    }
    let name = canonical(name);
    let Some(&(_, heading, see_also)) = TOPICS.iter().find(|t| t.0 == name) else {
        return Err(format!(
            "no help for `{name}`; try: {}, all",
            names().replace(' ', ", ")
        ));
    };
    let section = sections(&lines[end + 1..])
        .into_iter()
        .find(|s| s[0].starts_with(heading))
        .unwrap_or_else(|| panic!("help.txt has no section `{}`", heading));
    let mut s = String::new();
    let u = usage(&lines[..end], name);
    if !u.is_empty() {
        s.push_str(&join(&u));
        s.push('\n');
    }
    s.push_str(&join(section));
    if !see_also.is_empty() {
        let more: Vec<String> = see_also
            .iter()
            .map(|n| format!("devsrv help {n}"))
            .collect();
        s.push_str(&format!("\nMore: {}\n", more.join(", ")));
    }
    Ok(s)
}
