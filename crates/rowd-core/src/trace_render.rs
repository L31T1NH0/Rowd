//! Streaming human renderer: fields stay structured in the source JSONL.
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    path::Path,
};

#[derive(Default)]
pub struct Filter {
    pub component: Option<String>,
    pub share: Option<String>,
    pub errors: bool,
    pub connection: Option<String>,
    pub round: Option<String>,
    pub file: Option<String>,
    pub request: Option<String>,
}
impl Filter {
    pub fn matches(&self, v: &Value) -> bool {
        let ctx = &v["context"];
        (!self.errors || v["level"] == "error" || v["event"] == "INVARIANT_VIOLATION")
            && self.component.as_ref().is_none_or(|s| {
                v["component"]
                    .as_str()
                    .is_some_and(|c| c.eq_ignore_ascii_case(s))
            })
            && self
                .share
                .as_ref()
                .is_none_or(|s| ctx["share_name"] == *s || ctx["share_id"] == *s)
            && [
                (&self.connection, "connection_id"),
                (&self.round, "round_id"),
                (&self.file, "file_id"),
                (&self.request, "request_id"),
            ]
            .iter()
            .all(|(filter, key)| filter.as_ref().is_none_or(|s| ctx[*key] == *s))
    }
}
pub fn render(v: &Value) -> String {
    let ctx = &v["context"];
    let wall = v["wall_time"].as_str().unwrap_or("?");
    let mut line = format!(
        "{} [{}] [{}]",
        wall.get(11..23).unwrap_or(wall),
        v["level"].as_str().unwrap_or("trace").to_ascii_uppercase(),
        v["component"].as_str().unwrap_or("Trace")
    );
    if let Some(name) = ctx["share_name"]
        .as_str()
        .or_else(|| ctx["share_id"].as_str())
    {
        if let (Some(index), Some(total)) =
            (ctx["share_index"].as_u64(), ctx["share_total"].as_u64())
        {
            line.push_str(&format!(" [{index}/{total} {name}]"));
        } else {
            line.push_str(&format!(" [{name}]"));
        }
    }
    for (key, label) in [
        ("request_id", "req"),
        ("round_id", "round"),
        ("connection_id", "conn"),
        ("transfer_id", "tx"),
    ] {
        if let Some(id) = ctx[key].as_str() {
            line.push_str(&format!(" [{label}={id}]"));
        }
    }
    line.push_str(&format!(" {}", v["event"].as_str().unwrap_or("UNKNOWN")));
    if let Some(fields) = v["fields"].as_object() {
        let fields = fields
            .iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>();
        if !fields.is_empty() {
            line.push_str(" | ");
            line.push_str(&fields.join(" "));
        }
    }
    if v["level"] == "error" {
        line.push_str(&format!(
            " | source={}:{} {} thread={}",
            v["source"]["file"].as_str().unwrap_or("?"),
            v["source"]["line"],
            v["source"]["function"].as_str().unwrap_or("?"),
            v["source"]["thread"].as_str().unwrap_or("?")
        ));
    }
    line
}
pub fn show(path: &Path, filter: &Filter, output: &mut impl Write) -> Result<()> {
    let path = if path.join("Latest-trace").is_dir() {
        path.join("Latest-trace")
    } else {
        path.into()
    };
    let mut files = if path.is_dir() {
        fs::read_dir(&path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect::<Vec<_>>()
    } else {
        vec![path]
    };
    files.sort();
    for file in files {
        let mut reader = BufReader::new(File::open(&file)?);
        let mut line = String::new();
        let mut number = 0;
        while reader.read_line(&mut line)? > 0 {
            number += 1;
            match serde_json::from_str::<Value>(&line) {
                Ok(v) => {
                    if filter.matches(&v) {
                        writeln!(output, "{}", render(&v))?
                    }
                }
                Err(_) if !line.ends_with('\n') => writeln!(
                    output,
                    "[WARN] [Trace] INCOMPLETE_FINAL_RECORD | file={} line={number}",
                    file.display()
                )?,
                Err(error) => {
                    return Err(error).with_context(|| format!("{}:{number}", file.display()))
                }
            };
            line.clear();
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn human_and_filters() {
        let v = serde_json::json!({"wall_time":"2026-10-01T20:44:33.184Z","level":"trace","component":"Watcher","event":"OBSERVER_CALLBACK","context":{"share_name":"camera","share_index":3,"share_total":5,"connection_id":"31"},"fields":{"self_change":false}});
        assert!(render(&v).contains("20:44:33.184 [TRACE] [Watcher] [3/5 camera]"));
        assert!(Filter {
            share: Some("camera".into()),
            connection: Some("31".into()),
            ..Default::default()
        }
        .matches(&v));
        assert!(!Filter {
            errors: true,
            ..Default::default()
        }
        .matches(&v));
    }
}
