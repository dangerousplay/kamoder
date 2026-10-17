//! `kboot status`: the record, both slots and where they map, read-only.
//!
//! The JSON form is a fixed, flat shape written by hand (no serde in this
//! binary); strings go through [`json_str`].

use std::fmt::Write as _;

use bootctl::{choose, Loaded, SdSelector, Slot};

use crate::{Layout, Result};

/// Snapshot for display.
#[derive(Debug, Clone)]
pub struct Status {
    pub root: String,
    pub loaded: Loaded,
    pub last_file: Option<String>,
    /// What the next `select` would pick from the record alone.
    pub next: (Slot, bool),
}

/// Reads the selector without creating or writing anything.
pub fn read(layout: &Layout) -> Result<Status> {
    let loaded = SdSelector::new(layout.boot_dir()).load_detailed()?;
    let c = choose(&loaded.record);
    Ok(Status {
        root: layout.root().display().to_string(),
        loaded,
        last_file: layout.read_last(),
        next: (c.slot, c.trial),
    })
}

fn dir_of(layout: &Layout, slot: Slot) -> Option<String> {
    layout.slot_dir(slot).map(|d| d.display().to_string())
}

/// Human-readable, one fact per line.
pub fn human(layout: &Layout, st: &Status) -> String {
    let rec = &st.loaded.record;
    let mut out = String::new();
    let _ = writeln!(out, "root:        {}", st.root);
    match st.loaded.copy {
        Some(copy) => {
            let _ = writeln!(
                out,
                "record:      seq {} from sel.{copy} (valid: sel.0 {}, sel.1 {})",
                rec.seq,
                yes_no(st.loaded.valid[0]),
                yes_no(st.loaded.valid[1])
            );
        }
        None => {
            let _ = writeln!(out, "record:      none valid (legacy defaults)");
        }
    }
    let _ = writeln!(
        out,
        "last booted: {}",
        rec.last_booted.map_or("none", Slot::name)
    );
    let _ = writeln!(
        out,
        "boot/last:   {}",
        st.last_file.as_deref().unwrap_or("(none)")
    );
    for slot in Slot::ALL {
        let s = rec.slot(slot);
        let state = match (s.bootable, s.successful) {
            (false, _) => "unbootable".to_owned(),
            (true, true) => "committed".to_owned(),
            (true, false) => format!("trial, {} tries left", s.tries_left),
        };
        let dir = match dir_of(layout, slot) {
            Some(d) if layout.is_legacy(slot) => format!("{d} (legacy root)"),
            Some(d) => d,
            None => "(no init.sh)".to_owned(),
        };
        let _ = writeln!(
            out,
            "slot {slot}:      {state}, priority {}, version {:?}, dir {dir}",
            s.priority,
            s.version.as_str()
        );
    }
    let _ = writeln!(
        out,
        "next boot:   slot {}{}",
        st.next.0,
        if st.next.1 { " (trial)" } else { "" }
    );
    out
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

/// Machine-readable, one JSON object.
pub fn json(layout: &Layout, st: &Status) -> String {
    let rec = &st.loaded.record;
    let mut out = String::from("{");
    let _ = write!(out, "\"root\":{}", json_str(&st.root));
    let _ = write!(out, ",\"fresh\":{}", st.loaded.fresh);
    let _ = write!(out, ",\"seq\":{}", rec.seq);
    let _ = write!(
        out,
        ",\"copy\":{}",
        opt(st.loaded.copy.map(|c| c.to_string()))
    );
    let _ = write!(
        out,
        ",\"valid\":[{},{}]",
        st.loaded.valid[0], st.loaded.valid[1]
    );
    let _ = write!(
        out,
        ",\"last_booted\":{}",
        opt(rec.last_booted.map(|s| json_str(s.name())))
    );
    let _ = write!(
        out,
        ",\"last_file\":{}",
        opt(st.last_file.as_deref().map(json_str))
    );
    out.push_str(",\"slots\":{");
    for (i, slot) in Slot::ALL.into_iter().enumerate() {
        let s = rec.slot(slot);
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "\"{slot}\":{{\"bootable\":{},\"successful\":{},\"tries_left\":{},\"priority\":{},\
             \"version\":{},\"dir\":{},\"legacy\":{}}}",
            s.bootable,
            s.successful,
            s.tries_left,
            s.priority,
            json_str(s.version.as_str()),
            opt(dir_of(layout, slot).as_deref().map(json_str)),
            layout.is_legacy(slot)
        );
    }
    let _ = write!(
        out,
        "}},\"next\":{{\"slot\":\"{}\",\"trial\":{}}}}}",
        st.next.0, st.next.1
    );
    out.push('\n');
    out
}

fn opt(v: Option<String>) -> String {
    v.unwrap_or_else(|| "null".to_owned())
}

/// A JSON string literal (RFC 8259 escaping).
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escaping() {
        assert_eq!(json_str("a\"b\\c\n\u{1}é"), "\"a\\\"b\\\\c\\n\\u0001é\"");
    }
}
